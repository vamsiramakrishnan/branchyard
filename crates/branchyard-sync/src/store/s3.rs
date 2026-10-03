//! `s3://bucket/prefix`: Amazon S3 and S3-compatible stores (R2, MinIO,
//! Ceph) through the REST API, signed with SigV4.
//!
//! - Conditional writes: `If-None-Match: *` for a new object, `If-Match:
//!   <etag>` to replace or delete one (S3 has taken both on writes since
//!   August 2024 and `If-Match` on deletes since late 2024; R2 and recent
//!   MinIO take them too).
//! - Large objects go up as a multipart upload whose completion carries
//!   `If-None-Match: *`; the upload ID and each part's ETag are journaled,
//!   so a stopped upload continues where it was.
//! - Query options: `endpoint=http://host:port` (path-style addressing,
//!   for MinIO, R2 and the stand-in), `region=` (else `AWS_REGION`,
//!   `AWS_DEFAULT_REGION`, `us-east-1`; R2 uses `auto`), `path_style=true`,
//!   `profile=`, and `part_size=` in bytes (at least 5 MiB on S3; default
//!   8 MiB).

use std::sync::Arc;

use crate::auth::aws::AwsCredentials;
use crate::auth::sigv4::{self, Credentials, EMPTY_SHA256};
use crate::error::{Error, Result};
use crate::http::{send, Request, Response, Url};
use crate::store::xml;
use crate::store::{
    check_key, check_prefix, join, Entry, Generation, Object, ObjectStore, UploadJournal,
};
use crate::util::{parse_http_date, parse_rfc3339, uri_encode, xml_escape};
use branchyard::services::Clock;

pub const DEFAULT_PART: usize = 8 << 20;

enum Creds {
    Fixed(Credentials),
    Chain(AwsCredentials),
}

pub struct S3Store {
    bucket: String,
    prefix: String,
    base: Url,
    path_style: bool,
    region: String,
    creds: Arc<Creds>,
    part_size: usize,
    clock: Clock,
}

impl S3Store {
    pub fn new(bucket: &str, prefix: &str, query: &[(String, String)]) -> Result<S3Store> {
        let get = |k: &str| {
            query
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .filter(|v| !v.is_empty())
        };
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let region = get("region")
            .or_else(|| var("AWS_REGION"))
            .or_else(|| var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|| "us-east-1".into());
        let (base, path_style) = match get("endpoint") {
            Some(endpoint) => (
                Url::parse(endpoint.trim_end_matches('/'))?,
                get("path_style").as_deref() != Some("false"),
            ),
            None => match get("path_style").as_deref() == Some("true") {
                true => (
                    Url::parse(&format!("https://s3.{region}.amazonaws.com"))?,
                    true,
                ),
                false => (
                    Url::parse(&format!("https://{bucket}.s3.{region}.amazonaws.com"))?,
                    false,
                ),
            },
        };
        let part_size = match get("part_size") {
            Some(p) => p
                .parse()
                .map_err(|_| Error::config(format!("part_size={p} is not a number of bytes")))?,
            None => DEFAULT_PART,
        };
        Ok(S3Store {
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
            base,
            path_style,
            region,
            creds: Arc::new(Creds::Chain(AwsCredentials::new(get("profile")))),
            part_size: part_size.max(1),
            clock: Clock::system(),
        })
    }

    /// Sign with these credentials instead of looking for them.
    pub fn with_credentials(mut self, credentials: Credentials) -> S3Store {
        self.creds = Arc::new(Creds::Fixed(credentials));
        self
    }

    pub fn with_clock(mut self, clock: Clock) -> S3Store {
        self.clock = clock;
        self
    }

    pub fn with_part_size(mut self, bytes: usize) -> S3Store {
        self.part_size = bytes.max(1);
        self
    }

    fn base_path(&self) -> String {
        let root = self.base.path.trim_end_matches('/');
        match self.path_style {
            true => format!("{root}/{}", uri_encode(&self.bucket, false)),
            false => root.to_owned(),
        }
    }

    fn object_url(&self, key: &str, query: &str) -> Url {
        let path = format!(
            "{}/{}",
            self.base_path(),
            uri_encode(&join(&self.prefix, key), true)
        );
        self.base.with_path(&path, query)
    }

    fn bucket_url(&self, query: &str) -> Url {
        let path = match self.base_path() {
            p if p.is_empty() => "/".to_owned(),
            p => p,
        };
        self.base.with_path(&path, query)
    }

    fn call(&self, mut request: Request) -> Result<Response> {
        let now = self.clock.now();
        let creds = match self.creds.as_ref() {
            Creds::Fixed(c) => c.clone(),
            Creds::Chain(chain) => chain.get(now)?,
        };
        let hash = match &request.body {
            Some(body) if !body.is_empty() => sigv4::sha256_hex(body),
            _ => EMPTY_SHA256.to_owned(),
        };
        sigv4::sign(&mut request, &creds, &self.region, "s3", now, &hash, true);
        send(&request)
    }

    /// A non-2xx response's error; S3's `409 ConditionalRequestConflict`
    /// means a concurrent conditional write is under way: try again.
    fn fail(&self, response: &Response, what: &str) -> Error {
        let mut e = response.error(what);
        if response.status == 409 {
            e.kind = crate::Kind::Transient;
        }
        e
    }

    fn put(&self, key: &str, data: &[u8], condition: (&str, &str)) -> Result<Generation> {
        check_key(key)?;
        let response = self.call(
            Request::new("PUT", self.object_url(key, ""))
                .header(condition.0, condition.1)
                .header("Content-Type", "application/octet-stream")
                .body(data.to_vec()),
        )?;
        match response.status {
            200 | 201 => etag(&response),
            404 => Err(Error::precondition(format!("{key} does not exist"))),
            _ => Err(self.fail(&response, &format!("PUT {key}"))),
        }
    }

    fn abort(&self, key: &str, upload: &str) {
        let _ = self.call(Request::new(
            "DELETE",
            self.object_url(key, &format!("uploadId={}", uri_encode(upload, false))),
        ));
    }

    fn multipart(&self, key: &str, data: &[u8], journal: &dyn UploadJournal) -> Result<Generation> {
        let digest = crate::util::hex(&blake3::hash(data).as_bytes()[..16]);
        let mut state: Multipart = journal
            .load(key)
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|s: &Multipart| s.digest == digest && s.part_size == self.part_size)
            .unwrap_or_default();
        if state.upload.is_empty() {
            let response = self.call(
                Request::new("POST", self.object_url(key, "uploads"))
                    .header("Content-Type", "application/octet-stream")
                    .body(Vec::new()),
            )?;
            if !response.ok() {
                return Err(self.fail(&response, &format!("starting the upload of {key}")));
            }
            state = Multipart {
                digest: digest.clone(),
                part_size: self.part_size,
                upload: xml::text(&response.text(), "UploadId")
                    .ok_or_else(|| Error::refused("CreateMultipartUpload returned no UploadId"))?,
                parts: Vec::new(),
            };
            journal.save(key, &serde_json::to_string(&state)?);
        }
        let parts: Vec<&[u8]> = data.chunks(self.part_size).collect();
        for (i, part) in parts.iter().enumerate().skip(state.parts.len()) {
            let query = format!(
                "partNumber={}&uploadId={}",
                i + 1,
                uri_encode(&state.upload, false)
            );
            let response =
                self.call(Request::new("PUT", self.object_url(key, &query)).body(part.to_vec()))?;
            if response.status == 404 {
                // The upload was aborted or expired: start over next time.
                journal.clear(key);
                return Err(Error::transient(format!(
                    "the multipart upload of {key} is gone; starting over"
                )));
            }
            if !response.ok() {
                return Err(self.fail(&response, &format!("part {} of {key}", i + 1)));
            }
            state.parts.push(etag(&response)?);
            journal.save(key, &serde_json::to_string(&state)?);
        }
        let mut body = String::from("<CompleteMultipartUpload>");
        for (i, tag) in state.parts.iter().enumerate() {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                i + 1,
                xml_escape(tag)
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let response = self.call(
            Request::new(
                "POST",
                self.object_url(
                    key,
                    &format!("uploadId={}", uri_encode(&state.upload, false)),
                ),
            )
            .header("If-None-Match", "*")
            .header("Content-Type", "application/xml")
            .body(body.into_bytes()),
        )?;
        let text = response.text();
        if response.status == 412 {
            self.abort(key, &state.upload);
            journal.clear(key);
            return Err(Error::precondition(format!("{key} already exists")));
        }
        // CompleteMultipartUpload can answer 200 with an error in the body.
        if !response.ok() || text.contains("<Error>") {
            let mut e = self.fail(&response, &format!("completing the upload of {key}"));
            if response.ok() {
                e = Error::transient(format!("completing the upload of {key}: {text}"));
            }
            return Err(e);
        }
        journal.clear(key);
        match xml::text(&text, "ETag") {
            Some(tag) => Ok(tag),
            None => etag(&response),
        }
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Multipart {
    digest: String,
    part_size: usize,
    upload: String,
    parts: Vec<String>,
}

fn etag(response: &Response) -> Result<Generation> {
    response
        .header("etag")
        .map(str::to_owned)
        .ok_or_else(|| Error::refused("the response has no ETag"))
}

impl ObjectStore for S3Store {
    fn url(&self) -> String {
        let mut url = format!("s3://{}", self.bucket);
        if !self.prefix.is_empty() {
            url.push('/');
            url.push_str(&self.prefix);
        }
        url
    }

    fn get(&self, key: &str) -> Result<Object> {
        check_key(key)?;
        let response = self.call(Request::new("GET", self.object_url(key, "")))?;
        match response.status {
            200 => Ok(Object {
                generation: etag(&response)?,
                data: response.body,
            }),
            _ => Err(self.fail(&response, &format!("GET {key}"))),
        }
    }

    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        check_key(key)?;
        if len == 0 {
            return Ok(Vec::new());
        }
        let response = self.call(
            Request::new("GET", self.object_url(key, ""))
                .header("Range", format!("bytes={start}-{}", start + len - 1)),
        )?;
        match response.status {
            206 => Ok(response.body),
            200 => {
                let s = (start as usize).min(response.body.len());
                let e = s.saturating_add(len as usize).min(response.body.len());
                Ok(response.body[s..e].to_vec())
            }
            416 => Ok(Vec::new()),
            _ => Err(self.fail(&response, &format!("GET {key}"))),
        }
    }

    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        check_key(key)?;
        let response = self.call(Request::new("HEAD", self.object_url(key, "")))?;
        match response.status {
            200 => Ok(Some(Entry {
                key: key.to_owned(),
                size: response
                    .header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                generation: etag(&response)?,
                modified_ms: response.header("last-modified").and_then(parse_http_date),
            })),
            404 => Ok(None),
            _ => Err(self.fail(&response, &format!("HEAD {key}"))),
        }
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        self.put(key, data, ("If-None-Match", "*"))
    }

    fn put_if_match(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation> {
        self.put(key, data, ("If-Match", generation))
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        check_prefix(prefix)?;
        let full = match self.prefix.is_empty() {
            true => prefix.to_owned(),
            false => format!("{}/{prefix}", self.prefix),
        };
        let strip = match self.prefix.is_empty() {
            true => 0,
            false => self.prefix.len() + 1,
        };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = format!("list-type=2&prefix={}", uri_encode(&full, false));
            if let Some(t) = &token {
                query.push_str(&format!("&continuation-token={}", uri_encode(t, false)));
            }
            let response = self.call(Request::new("GET", self.bucket_url(&query)))?;
            if !response.ok() {
                return Err(self.fail(&response, &format!("listing {prefix}")));
            }
            let text = response.text();
            for item in xml::elements(&text, "Contents") {
                let Some(key) = xml::text(item, "Key") else {
                    continue;
                };
                if key.len() < strip {
                    continue;
                }
                out.push(Entry {
                    key: key[strip..].to_owned(),
                    size: xml::text(item, "Size")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    generation: xml::text(item, "ETag").unwrap_or_default(),
                    modified_ms: xml::text(item, "LastModified").and_then(|t| parse_rfc3339(&t)),
                });
            }
            match (
                xml::text(&text, "IsTruncated").as_deref(),
                xml::text(&text, "NextContinuationToken"),
            ) {
                (Some("true"), Some(next)) => token = Some(next),
                _ => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()> {
        check_key(key)?;
        let response = self.call(
            Request::new("DELETE", self.object_url(key, "")).header("If-Match", generation),
        )?;
        match response.status {
            200 | 204 => Ok(()),
            404 | 412 => Err(Error::precondition(format!(
                "{key} is not at generation {generation}"
            ))),
            _ => Err(self.fail(&response, &format!("DELETE {key}"))),
        }
    }

    fn resumable_put(
        &self,
        key: &str,
        data: &[u8],
        journal: &dyn UploadJournal,
    ) -> Result<Generation> {
        check_key(key)?;
        match data.len() > self.part_size {
            true => self.multipart(key, data, journal),
            false => self.put_if_absent(key, data),
        }
    }
}
