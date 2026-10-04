//! A Cloud Storage stand-in for the JSON API: objects with generations
//! from one counter, `ifGenerationMatch` on uploads and deletes (`0`:
//! only if absent), media downloads with ranges, listing with page
//! tokens, resumable upload sessions (chunks by `Content-Range`, status
//! queries by `bytes */N`), and a bearer token checked on every request.

use branchyard_support::LockExt as _;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::server::{Faults, MockRequest, MockResponse, MockServer};

pub const TOKEN: &str = "ya29.standin-token";

#[derive(Clone)]
struct Obj {
    data: Vec<u8>,
    generation: u64,
}

struct Session {
    name: String,
    if_generation: Option<u64>,
    total: Option<usize>,
    data: Vec<u8>,
    finished: Option<u64>,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Obj>,
    sessions: BTreeMap<String, Session>,
    next: u64,
}

pub struct MockGcs {
    pub server: MockServer,
    pub faults: Arc<Faults>,
    state: Arc<Mutex<State>>,
}

fn error(status: u16, message: &str) -> MockResponse {
    MockResponse::json(
        status,
        &serde_json::json!({"error": {"code": status, "message": message}}),
    )
}

fn resource(bucket: &str, name: &str, o: &Obj) -> serde_json::Value {
    serde_json::json!({
        "kind": "storage#object",
        "bucket": bucket,
        "name": name,
        "size": o.data.len().to_string(),
        "generation": o.generation.to_string(),
        "updated": "2015-08-30T12:36:00.000Z",
    })
}

impl MockGcs {
    /// A server holding `bucket`; `token` is required when given.
    pub fn start(bucket: &str, token: Option<&str>, page: usize) -> MockGcs {
        let state = Arc::new(Mutex::new(State {
            next: 1000,
            ..State::default()
        }));
        let faults = Arc::new(Faults::default());
        let base = Arc::new(Mutex::new(String::new()));
        let handler = {
            let (state, faults, bucket, token, base) = (
                state.clone(),
                faults.clone(),
                bucket.to_owned(),
                token.map(str::to_owned),
                base.clone(),
            );
            Arc::new(move |r: &MockRequest| {
                if let Some(t) = &token {
                    if r.header("authorization") != Some(&format!("Bearer {t}")) {
                        return error(401, "Invalid Credentials");
                    }
                }
                if let Some(fail) = faults.check(r) {
                    return fail;
                }
                let base = base.lock_recovering("gcs base url").clone();
                handle(&state, &bucket, page.max(1), &base, r)
            })
        };
        let server = MockServer::start(handler);
        *base.lock_recovering("gcs base url") = server.url.clone();
        MockGcs {
            server,
            faults,
            state,
        }
    }

    pub fn remote_url(&self, bucket: &str, prefix: &str) -> String {
        format!(
            "gs://{bucket}/{prefix}?endpoint={}",
            crate::util::uri_encode(&self.server.url, false)
        )
    }

    pub fn store(
        &self,
        bucket: &str,
        prefix: &str,
        part_size: usize,
    ) -> crate::store::gcs::GcsStore {
        crate::store::gcs::GcsStore::new(
            bucket,
            prefix,
            &[("endpoint".into(), self.server.url.clone())],
        )
        .expect("a GCS store")
        .with_auth(crate::auth::google::GoogleAuth::fixed(TOKEN))
        .with_part_size(part_size)
    }

    pub fn objects(&self) -> BTreeMap<String, Vec<u8>> {
        self.state
            .lock_recovering("state")
            .objects
            .iter()
            .map(|(k, o)| (k.clone(), o.data.clone()))
            .collect()
    }

    /// Bytes held by open (unfinished) resumable sessions.
    pub fn session_bytes(&self) -> Vec<usize> {
        self.state
            .lock_recovering("state")
            .sessions
            .values()
            .filter(|s| s.finished.is_none())
            .map(|s| s.data.len())
            .collect()
    }
}

fn check(s: &State, name: &str, want: Option<u64>) -> Option<MockResponse> {
    let have = s.objects.get(name).map(|o| o.generation);
    match want {
        Some(0) if have.is_some() => Some(error(412, "conditionNotMet")),
        Some(g) if g != 0 && have != Some(g) => Some(error(412, "conditionNotMet")),
        _ => None,
    }
}

fn handle(
    state: &Mutex<State>,
    bucket: &str,
    page: usize,
    base: &str,
    r: &MockRequest,
) -> MockResponse {
    let mut s = state.lock_recovering("state");
    let object_prefix = format!("/storage/v1/b/{bucket}/o/");
    let list_path = format!("/storage/v1/b/{bucket}/o");
    let upload_path = format!("/upload/storage/v1/b/{bucket}/o");
    let if_generation = match r.param("ifGenerationMatch") {
        None => None,
        Some(g) => match g.parse() {
            Ok(g) => Some(g),
            Err(_) => return error(400, "Invalid argument ifGenerationMatch"),
        },
    };
    if r.path == upload_path {
        if r.method == "POST" && r.param("uploadType").as_deref() == Some("media") {
            let name = r.param("name").unwrap_or_default();
            if let Some(fail) = check(&s, &name, if_generation) {
                return fail;
            }
            s.next += 1;
            let o = Obj {
                data: r.body.clone(),
                generation: s.next,
            };
            let res = resource(bucket, &name, &o);
            s.objects.insert(name, o);
            return MockResponse::json(200, &res);
        }
        if r.method == "POST" && r.param("uploadType").as_deref() == Some("resumable") {
            let name = r.param("name").unwrap_or_default();
            if let Some(fail) = check(&s, &name, if_generation) {
                return fail;
            }
            s.next += 1;
            let id = format!("session{}", s.next);
            s.sessions.insert(
                id.clone(),
                Session {
                    name,
                    if_generation,
                    total: r
                        .header("x-upload-content-length")
                        .and_then(|v| v.parse().ok()),
                    data: Vec::new(),
                    finished: None,
                },
            );
            return MockResponse::new(200).header(
                "Location",
                format!("{base}{upload_path}?uploadType=resumable&upload_id={id}"),
            );
        }
        if r.method == "PUT" {
            let id = r.param("upload_id").unwrap_or_default();
            let range = r.header("content-range").unwrap_or("").to_owned();
            let Some(session) = s.sessions.get_mut(&id) else {
                return error(404, "no such upload session");
            };
            if let Some(g) = session.finished {
                let o = s
                    .objects
                    .get(&s.sessions[&id].name)
                    .cloned()
                    .unwrap_or(Obj {
                        data: Vec::new(),
                        generation: g,
                    });
                let name = s.sessions[&id].name.clone();
                return MockResponse::json(200, &resource(bucket, &name, &o));
            }
            let spec = range.trim_start_matches("bytes ");
            let (span, total) = spec.split_once('/').unwrap_or((spec, "*"));
            if let Ok(t) = total.parse::<usize>() {
                session.total = Some(t);
            }
            if span != "*" {
                let start: usize = span
                    .split_once('-')
                    .and_then(|(a, _)| a.parse().ok())
                    .unwrap_or(0);
                if start != session.data.len() {
                    return error(400, "the chunk does not start where the session ends");
                }
                session.data.extend_from_slice(&r.body);
            }
            if session.total == Some(session.data.len()) && session.total.is_some() {
                let (name, want, data) = (
                    session.name.clone(),
                    session.if_generation,
                    session.data.clone(),
                );
                if let Some(fail) = check(&s, &name, want) {
                    s.sessions.remove(&id);
                    return fail;
                }
                s.next += 1;
                let o = Obj {
                    data,
                    generation: s.next,
                };
                let res = resource(bucket, &name, &o);
                s.objects.insert(name, o);
                let g = s.next;
                if let Some(session) = s.sessions.get_mut(&id) {
                    session.finished = Some(g);
                }
                return MockResponse::json(200, &res);
            }
            let held = s.sessions[&id].data.len();
            let response = MockResponse::new(308);
            return match held {
                0 => response,
                n => response.header("Range", format!("bytes=0-{}", n - 1)),
            };
        }
    }
    if r.path == list_path && r.method == "GET" {
        let prefix = r.param("prefix").unwrap_or_default();
        let after = r.param("pageToken").unwrap_or_default();
        let matching: Vec<(&String, &Obj)> = s
            .objects
            .iter()
            .filter(|(k, _)| {
                k.starts_with(&prefix) && (after.is_empty() || k.as_str() > after.as_str())
            })
            .collect();
        let items: Vec<serde_json::Value> = matching
            .iter()
            .take(page)
            .map(|(k, o)| resource(bucket, k, o))
            .collect();
        let mut out = serde_json::json!({"kind": "storage#objects", "items": items});
        if matching.len() > page {
            out["nextPageToken"] = serde_json::Value::from(matching[page - 1].0.as_str());
        }
        return MockResponse::json(200, &out);
    }
    if let Some(encoded) = r.path.strip_prefix(&object_prefix) {
        let name = branchyard_client::http::decode(encoded);
        match r.method.as_str() {
            "GET" => {
                let Some(o) = s.objects.get(&name) else {
                    return error(404, "No such object");
                };
                if r.param("alt").as_deref() != Some("media") {
                    return MockResponse::json(200, &resource(bucket, &name, o));
                }
                let response =
                    MockResponse::new(200).header("x-goog-generation", o.generation.to_string());
                if let Some(range) = r.header("range") {
                    let (a, z) = range
                        .trim_start_matches("bytes=")
                        .split_once('-')
                        .unwrap_or(("0", ""));
                    let a: usize = a.parse().unwrap_or(0);
                    if a >= o.data.len() {
                        return error(416, "range not satisfiable");
                    }
                    let z: usize = z.parse().unwrap_or(o.data.len() - 1).min(o.data.len() - 1);
                    let mut response = response.body(o.data[a..=z].to_vec());
                    response.status = 206;
                    return response;
                }
                return response.body(o.data.clone());
            }
            "DELETE" => {
                if !s.objects.contains_key(&name) {
                    return error(404, "No such object");
                }
                if let Some(fail) = check(&s, &name, if_generation) {
                    return fail;
                }
                s.objects.remove(&name);
                return MockResponse::new(204);
            }
            _ => {}
        }
    }
    error(400, &format!("unexpected {}", r.line()))
}
