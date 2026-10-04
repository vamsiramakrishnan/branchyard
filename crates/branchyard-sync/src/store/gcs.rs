//! `gs://bucket/prefix`: Google Cloud Storage through its JSON API.
//!
//! - Conditional writes and deletes: `ifGenerationMatch` (`0` for "only
//!   if absent"). The generation is the object's own.
//! - Large objects go up as a resumable upload session; the session URI is
//!   journaled, and a later try asks the session how much it holds
//!   (`Content-Range: bytes */N`) and continues from there.
//! - Auth: [`GoogleAuth`] (a token from `GOOGLE_OAUTH_ACCESS_TOKEN`, a
//!   service-account key, or the metadata server). With
//!   `STORAGE_EMULATOR_HOST` set (fake-gcs-server), requests go there,
//!   unauthenticated.
//! - Query options: `endpoint=` (another API root, such as an emulator's),
//!   `part_size=` in bytes for resumable uploads (a multiple of 256 KiB on
//!   the real service; default 8 MiB).

use std::sync::Arc;

use crate::auth::google::GoogleAuth;
use crate::error::{Error, Result};
use crate::http::{send, Request, Response, Url};
use crate::store::{
    check_key, check_prefix, join, Entry, Generation, Object, ObjectStore, UploadJournal,
};
use crate::util::uri_encode;
use branchyard::services::Clock;
use branchyard_support::time::parse_rfc3339;

pub const DEFAULT_PART: usize = 8 << 20;

pub struct GcsStore {
    bucket: String,
    prefix: String,
    base: Url,
    auth: Arc<GoogleAuth>,
    part_size: usize,
    clock: Clock,
}

impl GcsStore {
    pub fn new(bucket: &str, prefix: &str, query: &[(String, String)]) -> Result<GcsStore> {
        let get = |k: &str| {
            query
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .filter(|v| !v.is_empty())
        };
        let emulator = std::env::var("STORAGE_EMULATOR_HOST")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let (endpoint, auth) = match (get("endpoint"), emulator) {
            (Some(e), _) => (e, GoogleAuth::from_env()?),
            (None, Some(host)) => {
                let host = match host.starts_with("http") {
                    true => host,
                    false => format!("http://{host}"),
                };
                (host, GoogleAuth::anonymous())
            }
            (None, None) => (
                "https://storage.googleapis.com".to_owned(),
                GoogleAuth::from_env()?,
            ),
        };
        let part_size = match get("part_size") {
            Some(p) => p
                .parse()
                .map_err(|_| Error::config(format!("part_size={p} is not a number of bytes")))?,
            None => DEFAULT_PART,
        };
        Ok(GcsStore {
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
            base: Url::parse(endpoint.trim_end_matches('/'))?,
            auth: Arc::new(auth),
            part_size: part_size.max(1),
            clock: Clock::system(),
        })
    }

    pub fn with_auth(mut self, auth: GoogleAuth) -> GcsStore {
        self.auth = Arc::new(auth);
        self
    }

    pub fn with_part_size(mut self, bytes: usize) -> GcsStore {
        self.part_size = bytes.max(1);
        self
    }

    fn root(&self) -> String {
        self.base.path.trim_end_matches('/').to_owned()
    }

    fn object_url(&self, key: &str, query: &str) -> Url {
        self.base.with_path(
            &format!(
                "{}/storage/v1/b/{}/o/{}",
                self.root(),
                uri_encode(&self.bucket, false),
                uri_encode(&join(&self.prefix, key), false)
            ),
            query,
        )
    }

    fn upload_url(&self, query: &str) -> Url {
        self.base.with_path(
            &format!(
                "{}/upload/storage/v1/b/{}/o",
                self.root(),
                uri_encode(&self.bucket, false)
            ),
            query,
        )
    }

    fn call(&self, mut request: Request) -> Result<Response> {
        if let Some(header) = self.auth.header(self.clock.now())? {
            request = request.header("Authorization", header);
        }
        send(&request)
    }

    fn upload(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation> {
        check_key(key)?;
        if generation.parse::<u64>().is_err() {
            return Err(Error::precondition(format!(
                "{key}: {generation:?} is not a Cloud Storage generation"
            )));
        }
        let query = format!(
            "uploadType=media&name={}&ifGenerationMatch={}",
            uri_encode(&join(&self.prefix, key), false),
            uri_encode(generation, false)
        );
        let response = self.call(
            Request::new("POST", self.upload_url(&query))
                .header("Content-Type", "application/octet-stream")
                .body(data.to_vec()),
        )?;
        match response.status {
            200 | 201 => generation_of(&response.json()?),
            404 if generation != "0" => Err(Error::precondition(format!("{key} does not exist"))),
            _ => Err(response.error(&format!("upload of {key}"))),
        }
    }

    /// Ask a session how many bytes it holds, or whether it finished.
    fn session_status(&self, session: &Url, total: usize) -> Result<Progress> {
        let response = self.call(
            Request::new("PUT", session.clone())
                .header("Content-Range", format!("bytes */{total}"))
                .body(Vec::new()),
        )?;
        match response.status {
            200 | 201 => Ok(Progress::Done(generation_of(&response.json()?)?)),
            308 => Ok(Progress::Have(persisted(&response))),
            404 | 410 => Ok(Progress::Gone),
            412 => Err(Error::precondition("the object already exists")),
            _ => Err(response.error("resumable upload status")),
        }
    }
}

enum Progress {
    Have(usize),
    Done(Generation),
    Gone,
}

/// Bytes a session holds, from a 308's `Range: bytes=0-N`.
fn persisted(response: &Response) -> usize {
    response
        .header("range")
        .and_then(|r| r.rsplit('-').next())
        .and_then(|n| n.trim().parse::<usize>().ok())
        .map(|n| n + 1)
        .unwrap_or(0)
}

fn generation_of(value: &serde_json::Value) -> Result<Generation> {
    match value.get("generation") {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(serde_json::Value::Number(n)) => Ok(n.to_string()),
        _ => Err(Error::refused("the object resource has no generation")),
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Session {
    digest: String,
    session: String,
}

impl ObjectStore for GcsStore {
    fn url(&self) -> String {
        let mut url = format!("gs://{}", self.bucket);
        if !self.prefix.is_empty() {
            url.push('/');
            url.push_str(&self.prefix);
        }
        url
    }

    fn get(&self, key: &str) -> Result<Object> {
        check_key(key)?;
        let response = self.call(Request::new("GET", self.object_url(key, "alt=media")))?;
        match response.status {
            200 => Ok(Object {
                generation: response
                    .header("x-goog-generation")
                    .map(str::to_owned)
                    .ok_or_else(|| Error::refused("the response has no x-goog-generation"))?,
                data: response.body,
            }),
            _ => Err(response.error(&format!("GET {key}"))),
        }
    }

    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        check_key(key)?;
        if len == 0 {
            return Ok(Vec::new());
        }
        let response = self.call(
            Request::new("GET", self.object_url(key, "alt=media"))
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
            _ => Err(response.error(&format!("GET {key}"))),
        }
    }

    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        check_key(key)?;
        let response = self.call(Request::new("GET", self.object_url(key, "")))?;
        match response.status {
            200 => {
                let value = response.json()?;
                Ok(Some(entry(key, &value)?))
            }
            404 => Ok(None),
            _ => Err(response.error(&format!("stat {key}"))),
        }
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        self.upload(key, data, "0")
    }

    fn put_if_match(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation> {
        if generation == "0" {
            return Err(Error::precondition(format!("{key} does not exist")));
        }
        self.upload(key, data, generation)
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        check_prefix(prefix)?;
        let full = join_prefix(&self.prefix, prefix);
        let strip = match self.prefix.is_empty() {
            true => 0,
            false => self.prefix.len() + 1,
        };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = format!("prefix={}", uri_encode(&full, false));
            if let Some(t) = &token {
                query.push_str(&format!("&pageToken={}", uri_encode(t, false)));
            }
            let url = self.base.with_path(
                &format!(
                    "{}/storage/v1/b/{}/o",
                    self.root(),
                    uri_encode(&self.bucket, false)
                ),
                &query,
            );
            let response = self.call(Request::new("GET", url))?;
            if !response.ok() {
                return Err(response.error(&format!("listing {prefix}")));
            }
            let value = response.json()?;
            for item in value
                .get("items")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.len() < strip {
                    continue;
                }
                out.push(entry(&name[strip..], item)?);
            }
            match value.get("nextPageToken").and_then(|v| v.as_str()) {
                Some(next) if !next.is_empty() => token = Some(next.to_owned()),
                _ => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()> {
        check_key(key)?;
        if generation.parse::<u64>().is_err() {
            return Err(Error::precondition(format!(
                "{key}: {generation:?} is not a Cloud Storage generation"
            )));
        }
        let response = self.call(Request::new(
            "DELETE",
            self.object_url(
                key,
                &format!("ifGenerationMatch={}", uri_encode(generation, false)),
            ),
        ))?;
        match response.status {
            200 | 204 => Ok(()),
            404 | 412 => Err(Error::precondition(format!(
                "{key} is not at generation {generation}"
            ))),
            _ => Err(response.error(&format!("DELETE {key}"))),
        }
    }

    fn resumable_put(
        &self,
        key: &str,
        data: &[u8],
        journal: &dyn UploadJournal,
    ) -> Result<Generation> {
        check_key(key)?;
        if data.len() <= self.part_size {
            return self.put_if_absent(key, data);
        }
        let digest = crate::util::hex(&blake3::hash(data).as_bytes()[..16]);
        let saved: Option<Session> = journal
            .load(key)
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|s: &Session| s.digest == digest);
        let mut offset = 0;
        let session = match saved {
            Some(s) => {
                let url = Url::parse(&s.session)?;
                match self.session_status(&url, data.len())? {
                    Progress::Done(generation) => {
                        journal.clear(key);
                        return Ok(generation);
                    }
                    Progress::Have(n) => {
                        offset = n;
                        Some(url)
                    }
                    Progress::Gone => None,
                }
            }
            None => None,
        };
        let session = match session {
            Some(url) => url,
            None => {
                let query = format!(
                    "uploadType=resumable&name={}&ifGenerationMatch=0",
                    uri_encode(&join(&self.prefix, key), false)
                );
                let response = self.call(
                    Request::new("POST", self.upload_url(&query))
                        .header("X-Upload-Content-Type", "application/octet-stream")
                        .header("X-Upload-Content-Length", data.len().to_string())
                        .body(Vec::new()),
                )?;
                if response.status == 412 {
                    return Err(Error::precondition(format!("{key} already exists")));
                }
                if !response.ok() {
                    return Err(response.error(&format!("starting the upload of {key}")));
                }
                let location = response
                    .header("location")
                    .ok_or_else(|| Error::refused("the resumable upload has no Location"))?
                    .to_owned();
                journal.save(
                    key,
                    &serde_json::to_string(&Session {
                        digest,
                        session: location.clone(),
                    })?,
                );
                Url::parse(&location)?
            }
        };
        loop {
            let end = (offset + self.part_size).min(data.len());
            let response = self.call(
                Request::new("PUT", session.clone())
                    .header(
                        "Content-Range",
                        format!("bytes {offset}-{}/{}", end - 1, data.len()),
                    )
                    .body(data[offset..end].to_vec()),
            )?;
            match response.status {
                200 | 201 => {
                    journal.clear(key);
                    return generation_of(&response.json()?);
                }
                308 => offset = persisted(&response).max(offset.min(end)),
                412 => {
                    journal.clear(key);
                    return Err(Error::precondition(format!("{key} already exists")));
                }
                404 | 410 => {
                    journal.clear(key);
                    return Err(Error::transient(format!(
                        "the upload session for {key} expired; starting over"
                    )));
                }
                _ => return Err(response.error(&format!("uploading {key}"))),
            }
            if offset >= data.len() {
                return Err(Error::transient(format!(
                    "the upload session for {key} holds every byte but did not finish"
                )));
            }
        }
    }
}

fn join_prefix(store: &str, prefix: &str) -> String {
    match store.is_empty() {
        true => prefix.to_owned(),
        false => format!("{store}/{prefix}"),
    }
}

fn entry(key: &str, value: &serde_json::Value) -> Result<Entry> {
    Ok(Entry {
        key: key.to_owned(),
        size: value
            .get("size")
            .and_then(|v| {
                v.as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| v.as_u64())
            })
            .unwrap_or(0),
        generation: generation_of(value)?,
        modified_ms: value
            .get("updated")
            .and_then(|v| v.as_str())
            .and_then(|t| parse_rfc3339(t).ok()),
    })
}
