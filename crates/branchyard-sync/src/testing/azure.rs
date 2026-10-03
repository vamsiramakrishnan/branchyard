//! An Azure Blob stand-in: Azurite-style paths (`/<account>/<container>/<blob>`),
//! Shared Key checked on every request, block blobs with ETags,
//! `If-None-Match: *` (answered `409 BlobAlreadyExists`, as Azure does)
//! and `If-Match` on puts, block lists and deletes, `x-ms-range`, staged
//! blocks, and listing with markers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::server::{Faults, MockRequest, MockResponse, MockServer};
use crate::auth::azure;
use crate::store::xml;
use crate::util::xml_escape;

pub const ACCOUNT: &str = "devstoreaccount1";
/// Azurite's well-known development key.
pub const KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

#[derive(Clone)]
struct Obj {
    data: Vec<u8>,
    etag: String,
}

#[derive(Default)]
struct State {
    blobs: BTreeMap<String, Obj>,
    staged: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    next: u64,
}

pub struct MockAzure {
    pub server: MockServer,
    pub faults: Arc<Faults>,
    state: Arc<Mutex<State>>,
}

fn error(status: u16, code: &str) -> MockResponse {
    MockResponse::xml(
        status,
        format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code></Error>"),
    )
}

impl MockAzure {
    pub fn start(container: &str, page: usize) -> MockAzure {
        let state = Arc::new(Mutex::new(State::default()));
        let faults = Arc::new(Faults::default());
        let handler = {
            let (state, faults, container) = (state.clone(), faults.clone(), container.to_owned());
            Arc::new(move |r: &MockRequest| {
                if let Err(why) = azure::verify(&r.as_request(), ACCOUNT, KEY) {
                    return error(403, &format!("AuthenticationFailed {why}"));
                }
                if let Some(fail) = faults.check(r) {
                    return fail;
                }
                handle(&state, &container, page.max(1), r)
            })
        };
        MockAzure {
            server: MockServer::start(handler),
            faults,
            state,
        }
    }

    pub fn endpoint(&self) -> String {
        format!("{}/{ACCOUNT}", self.server.url)
    }

    pub fn store(
        &self,
        container: &str,
        prefix: &str,
        part_size: usize,
    ) -> crate::store::azure::AzureStore {
        crate::store::azure::AzureStore::new(
            ACCOUNT,
            container,
            prefix,
            &[("endpoint".into(), self.endpoint())],
        )
        .expect("an Azure store")
        .with_credential(azure::Credential::SharedKey { key: KEY.into() })
        .with_part_size(part_size)
    }

    pub fn objects(&self) -> BTreeMap<String, Vec<u8>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .blobs
            .iter()
            .map(|(k, o)| (k.clone(), o.data.clone()))
            .collect()
    }

    /// Blocks staged and not yet committed, per blob.
    pub fn staged(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .staged
            .values()
            .map(BTreeMap::len)
            .sum()
    }
}

fn precondition(s: &State, name: &str, r: &MockRequest) -> Option<MockResponse> {
    let current = s.blobs.get(name);
    if r.header("if-none-match") == Some("*") && current.is_some() {
        return Some(error(409, "BlobAlreadyExists"));
    }
    if let Some(want) = r.header("if-match") {
        match current {
            None => return Some(error(404, "BlobNotFound")),
            Some(o) if o.etag != want => return Some(error(412, "ConditionNotMet")),
            _ => {}
        }
    }
    None
}

fn commit(s: &mut State, name: &str, data: Vec<u8>) -> String {
    s.next += 1;
    let etag = format!("\"0x8D{:012X}\"", s.next);
    s.blobs.insert(
        name.to_owned(),
        Obj {
            data,
            etag: etag.clone(),
        },
    );
    etag
}

fn handle(state: &Mutex<State>, container: &str, page: usize, r: &MockRequest) -> MockResponse {
    let path = r.decoded_path();
    let rest = path
        .trim_start_matches('/')
        .strip_prefix(&format!("{ACCOUNT}/"))
        .unwrap_or("")
        .to_owned();
    let (c, name) = match rest.split_once('/') {
        Some((c, n)) => (c.to_owned(), n.to_owned()),
        None => (rest.clone(), String::new()),
    };
    if c != container {
        return error(404, "ContainerNotFound");
    }
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    if name.is_empty() {
        if r.method == "GET" && r.param("comp").as_deref() == Some("list") {
            let prefix = r.param("prefix").unwrap_or_default();
            let after = r.param("marker").unwrap_or_default();
            let matching: Vec<(&String, &Obj)> = s
                .blobs
                .iter()
                .filter(|(k, _)| {
                    k.starts_with(&prefix) && (after.is_empty() || k.as_str() > after.as_str())
                })
                .collect();
            let mut out = String::from("<?xml version=\"1.0\"?><EnumerationResults><Blobs>");
            for (k, o) in matching.iter().take(page) {
                out.push_str(&format!(
                    "<Blob><Name>{}</Name><Properties><Last-Modified>Sun, 30 Aug 2015 12:36:00 GMT</Last-Modified><Etag>{}</Etag><Content-Length>{}</Content-Length></Properties></Blob>",
                    xml_escape(k),
                    xml_escape(&o.etag),
                    o.data.len()
                ));
            }
            out.push_str("</Blobs>");
            match matching.len() > page {
                true => out.push_str(&format!(
                    "<NextMarker>{}</NextMarker>",
                    xml_escape(matching[page - 1].0)
                )),
                false => out.push_str("<NextMarker />"),
            }
            out.push_str("</EnumerationResults>");
            return MockResponse::xml(200, out);
        }
        return error(400, "InvalidQueryParameterValue");
    }
    match (r.method.as_str(), r.param("comp").as_deref()) {
        ("GET" | "HEAD", None) => {
            let Some(o) = s.blobs.get(&name) else {
                return error(404, "BlobNotFound");
            };
            if let Some(range) = r.header("x-ms-range") {
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
        ("PUT", None) => {
            if r.header("x-ms-blob-type") != Some("BlockBlob") {
                return error(400, "MissingRequiredHeader");
            }
            if let Some(fail) = precondition(&s, &name, r) {
                return fail;
            }
            let etag = commit(&mut s, &name, r.body.clone());
            MockResponse::new(201).header("ETag", etag)
        }
        ("PUT", Some("block")) => {
            let id = r.param("blockid").unwrap_or_default();
            s.staged.entry(name).or_default().insert(id, r.body.clone());
            MockResponse::new(201)
        }
        ("PUT", Some("blocklist")) => {
            if let Some(fail) = precondition(&s, &name, r) {
                return fail;
            }
            let body = String::from_utf8_lossy(&r.body).into_owned();
            let staged = s.staged.get(&name).cloned().unwrap_or_default();
            let mut data = Vec::new();
            for id in xml::elements(&body, "Latest") {
                match staged.get(&xml::unescape(id)) {
                    Some(block) => data.extend_from_slice(block),
                    None => return error(400, "InvalidBlockList"),
                }
            }
            s.staged.remove(&name);
            let etag = commit(&mut s, &name, data);
            MockResponse::new(201).header("ETag", etag)
        }
        ("DELETE", None) => {
            match (s.blobs.get(&name), r.header("if-match")) {
                (None, _) => return error(404, "BlobNotFound"),
                (Some(o), Some(m)) if o.etag != m => return error(412, "ConditionNotMet"),
                _ => {}
            }
            s.blobs.remove(&name);
            MockResponse::new(202)
        }
        _ => error(400, "UnsupportedHttpVerb"),
    }
}
