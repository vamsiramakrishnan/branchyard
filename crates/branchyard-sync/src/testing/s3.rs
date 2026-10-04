//! An S3 stand-in: path-style buckets, SigV4 checked on every request,
//! objects with content ETags, `If-None-Match: *` and `If-Match` on puts,
//! deletes and multipart completion, ranges, ListObjectsV2 with paging,
//! and multipart uploads.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::server::{Faults, MockRequest, MockResponse, MockServer};
use crate::auth::sigv4::{self, Credentials};
use crate::store::xml;
use crate::util::xml_escape;

pub const ACCESS_KEY: &str = "AKIDSTANDIN";
pub const SECRET_KEY: &str = "standin/secret/key";

#[derive(Clone)]
struct Obj {
    data: Vec<u8>,
    etag: String,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Obj>,
    uploads: BTreeMap<String, (String, BTreeMap<u32, Obj>)>,
    next_upload: u64,
}

pub struct MockS3 {
    pub server: MockServer,
    pub faults: Arc<Faults>,
    state: Arc<Mutex<State>>,
}

fn etag_of(data: &[u8]) -> String {
    format!("\"{}\"", hex::encode(&blake3::hash(data).as_bytes()[..16]))
}

fn error(status: u16, code: &str) -> MockResponse {
    MockResponse::xml(
        status,
        format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code></Error>"),
    )
}

impl MockS3 {
    /// A server holding bucket `bucket`; `page` keys per list page.
    pub fn start(bucket: &str, page: usize) -> MockS3 {
        let state = Arc::new(Mutex::new(State::default()));
        let faults = Arc::new(Faults::default());
        let handler = {
            let (state, faults, bucket) = (state.clone(), faults.clone(), bucket.to_owned());
            Arc::new(move |r: &MockRequest| {
                if let Some(fail) = faults.check(r) {
                    return fail;
                }
                handle(&state, &bucket, page.max(1), r)
            })
        };
        MockS3 {
            server: MockServer::start(handler),
            faults,
            state,
        }
    }

    pub fn credentials() -> Credentials {
        Credentials {
            access_key: ACCESS_KEY.into(),
            secret_key: SECRET_KEY.into(),
            session_token: None,
            expires_ms: None,
        }
    }

    /// The URL of a remote in this bucket.
    pub fn remote_url(&self, bucket: &str, prefix: &str) -> String {
        format!(
            "s3://{bucket}/{prefix}?endpoint={}&region=us-east-1",
            crate::util::uri_encode(&self.server.url, false)
        )
    }

    /// A store on this server with the stand-in's credentials.
    pub fn store(&self, bucket: &str, prefix: &str, part_size: usize) -> crate::store::s3::S3Store {
        crate::store::s3::S3Store::new(
            bucket,
            prefix,
            &[
                ("endpoint".into(), self.server.url.clone()),
                ("region".into(), "us-east-1".into()),
            ],
        )
        .expect("an S3 store")
        .with_credentials(MockS3::credentials())
        .with_part_size(part_size)
    }

    /// Every key and its bytes, as stored.
    pub fn objects(&self) -> BTreeMap<String, Vec<u8>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .objects
            .iter()
            .map(|(k, o)| (k.clone(), o.data.clone()))
            .collect()
    }

    /// Change a stored object's bytes in place (to test integrity checks).
    pub fn corrupt(&self, key: &str) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(o) = s.objects.get_mut(key) {
            if let Some(b) = o.data.last_mut() {
                *b ^= 0x55;
            }
        }
    }

    pub fn uploads_open(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .uploads
            .len()
    }
}

fn handle(state: &Mutex<State>, bucket: &str, page: usize, r: &MockRequest) -> MockResponse {
    // Every request is signed, and the payload hash matches the body.
    let hash = sigv4::sha256_hex(&r.body);
    if r.header("x-amz-content-sha256") != Some(hash.as_str()) {
        return error(400, "XAmzContentSHA256Mismatch");
    }
    if let Err(why) = sigv4::verify(&r.as_request(), SECRET_KEY, &hash) {
        return error(403, &format!("SignatureDoesNotMatch: {why}"));
    }
    let path = r.decoded_path();
    let rest = path.trim_start_matches('/');
    let (b, key) = match rest.split_once('/') {
        Some((b, k)) => (b, k.to_owned()),
        None => (rest, String::new()),
    };
    if b != bucket {
        return error(404, "NoSuchBucket");
    }
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    if key.is_empty() {
        if r.method == "GET" && r.param("list-type").as_deref() == Some("2") {
            return list(&s, page, r);
        }
        return error(400, "InvalidRequest");
    }
    match r.method.as_str() {
        "GET" | "HEAD" => {
            let Some(o) = s.objects.get(&key) else {
                return error(404, "NoSuchKey");
            };
            if let Some(range) = r.header("range") {
                let (a, z) = range
                    .trim_start_matches("bytes=")
                    .split_once('-')
                    .unwrap_or(("0", ""));
                let a: usize = a.parse().unwrap_or(0);
                if a >= o.data.len() {
                    return error(416, "InvalidRange");
                }
                let z: usize = z.parse().unwrap_or(o.data.len() - 1).min(o.data.len() - 1);
                return MockResponse::new(206)
                    .header("ETag", o.etag.clone())
                    .body(o.data[a..=z].to_vec());
            }
            let response = MockResponse::new(200)
                .header("ETag", o.etag.clone())
                .header("Last-Modified", "Sun, 30 Aug 2015 12:36:00 GMT");
            match r.method.as_str() {
                "HEAD" => response.header("Content-Length", o.data.len().to_string()),
                _ => response.body(o.data.clone()),
            }
        }
        "PUT" if r.has_param("partNumber") => {
            let n: u32 = r
                .param("partNumber")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            let id = r.param("uploadId").unwrap_or_default();
            let Some((_, parts)) = s.uploads.get_mut(&id) else {
                return error(404, "NoSuchUpload");
            };
            let etag = etag_of(&r.body);
            parts.insert(
                n,
                Obj {
                    data: r.body.clone(),
                    etag: etag.clone(),
                },
            );
            MockResponse::new(200).header("ETag", etag)
        }
        "PUT" => {
            if let Some(resp) = precondition(&s, &key, r) {
                return resp;
            }
            let etag = etag_of(&r.body);
            s.objects.insert(
                key,
                Obj {
                    data: r.body.clone(),
                    etag: etag.clone(),
                },
            );
            MockResponse::new(200).header("ETag", etag)
        }
        "POST" if r.has_param("uploads") => {
            s.next_upload += 1;
            let id = format!("upload-{}", s.next_upload);
            s.uploads.insert(id.clone(), (key, BTreeMap::new()));
            MockResponse::xml(
                200,
                format!(
                    "<InitiateMultipartUploadResult><Bucket>{bucket}</Bucket><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
                ),
            )
        }
        "POST" if r.has_param("uploadId") => {
            let id = r.param("uploadId").unwrap_or_default();
            if !s.uploads.contains_key(&id) {
                return error(404, "NoSuchUpload");
            }
            if let Some(resp) = precondition(&s, &key, r) {
                return resp;
            }
            let body = String::from_utf8_lossy(&r.body).into_owned();
            let (_, parts) = s.uploads.get(&id).cloned().unwrap_or_default();
            let mut data = Vec::new();
            let listed = xml::elements(&body, "Part");
            for part in &listed {
                let n: u32 = xml::text(part, "PartNumber")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let tag = xml::text(part, "ETag").unwrap_or_default();
                match parts.get(&n) {
                    Some(p) if p.etag == tag => data.extend_from_slice(&p.data),
                    _ => return error(400, "InvalidPart"),
                }
            }
            s.uploads.remove(&id);
            let etag = format!(
                "\"{}-{}\"",
                hex::encode(&blake3::hash(&data).as_bytes()[..16]),
                listed.len()
            );
            s.objects.insert(
                key.clone(),
                Obj {
                    data,
                    etag: etag.clone(),
                },
            );
            MockResponse::xml(
                200,
                format!(
                    "<CompleteMultipartUploadResult><Key>{}</Key><ETag>{}</ETag></CompleteMultipartUploadResult>",
                    xml_escape(&key),
                    xml_escape(&etag)
                ),
            )
        }
        "DELETE" if r.has_param("uploadId") => {
            s.uploads.remove(&r.param("uploadId").unwrap_or_default());
            MockResponse::new(204)
        }
        "DELETE" => {
            match (s.objects.get(&key), r.header("if-match")) {
                (None, _) => return error(404, "NoSuchKey"),
                (Some(o), Some(m)) if o.etag != m => return error(412, "PreconditionFailed"),
                _ => {}
            }
            s.objects.remove(&key);
            MockResponse::new(204)
        }
        _ => error(405, "MethodNotAllowed"),
    }
}

fn precondition(s: &State, key: &str, r: &MockRequest) -> Option<MockResponse> {
    let current = s.objects.get(key);
    if r.header("if-none-match") == Some("*") && current.is_some() {
        return Some(error(412, "PreconditionFailed"));
    }
    if let Some(want) = r.header("if-match") {
        match current {
            None => return Some(error(404, "NoSuchKey")),
            Some(o) if o.etag != want => return Some(error(412, "PreconditionFailed")),
            _ => {}
        }
    }
    None
}

fn list(s: &State, page: usize, r: &MockRequest) -> MockResponse {
    let prefix = r.param("prefix").unwrap_or_default();
    let after = r.param("continuation-token").unwrap_or_default();
    let matching: Vec<(&String, &Obj)> = s
        .objects
        .iter()
        .filter(|(k, _)| {
            k.starts_with(&prefix) && (after.is_empty() || k.as_str() > after.as_str())
        })
        .collect();
    let mut out = String::from("<ListBucketResult>");
    for (k, o) in matching.iter().take(page) {
        out.push_str(&format!(
            "<Contents><Key>{}</Key><Size>{}</Size><ETag>{}</ETag><LastModified>2015-08-30T12:36:00.000Z</LastModified></Contents>",
            xml_escape(k),
            o.data.len(),
            xml_escape(&o.etag)
        ));
    }
    if matching.len() > page {
        out.push_str(&format!(
            "<IsTruncated>true</IsTruncated><NextContinuationToken>{}</NextContinuationToken>",
            xml_escape(matching[page - 1].0)
        ));
    } else {
        out.push_str("<IsTruncated>false</IsTruncated>");
    }
    out.push_str("</ListBucketResult>");
    MockResponse::xml(200, out)
}
