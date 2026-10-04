//! `az://account/container/prefix`: Azure Blob Storage through its REST
//! API.
//!
//! - Conditional writes and deletes: `If-None-Match: *` and `If-Match:
//!   <etag>`.
//! - Large objects go up as staged blocks (`Put Block`) committed by one
//!   `Put Block List` that carries the condition. Staged blocks are
//!   journaled by count, so a later try stages only the rest (Azure keeps
//!   uncommitted blocks for a week).
//! - Auth: [`Credential`]: Shared Key (`AZURE_STORAGE_KEY`), a SAS token
//!   (`AZURE_STORAGE_SAS_TOKEN`), or a managed identity.
//! - Query options: `endpoint=` (Azurite: `http://127.0.0.1:10000/devstoreaccount1`),
//!   `part_size=` in bytes (default 8 MiB).

use std::sync::Arc;

use crate::auth::azure::{self, Credential, VERSION};
use crate::error::{Error, Result};
use crate::http::{send, Request, Response, Url};
use crate::store::xml;
use crate::store::{
    check_key, check_prefix, join, Entry, Generation, Object, ObjectStore, UploadJournal,
};
use crate::util::{b64, uri_encode};
use branchyard::services::Clock;
use branchyard_support::time::{http_date, parse_http_date};

pub const DEFAULT_PART: usize = 8 << 20;

pub struct AzureStore {
    account: String,
    container: String,
    prefix: String,
    base: Url,
    credential: Arc<Credential>,
    part_size: usize,
    clock: Clock,
}

impl AzureStore {
    pub fn new(
        account: &str,
        container: &str,
        prefix: &str,
        query: &[(String, String)],
    ) -> Result<AzureStore> {
        let get = |k: &str| {
            query
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .filter(|v| !v.is_empty())
        };
        let endpoint =
            get("endpoint").unwrap_or_else(|| format!("https://{account}.blob.core.windows.net"));
        let part_size = match get("part_size") {
            Some(p) => p
                .parse()
                .map_err(|_| Error::config(format!("part_size={p} is not a number of bytes")))?,
            None => DEFAULT_PART,
        };
        Ok(AzureStore {
            account: account.to_owned(),
            container: container.to_owned(),
            prefix: prefix.to_owned(),
            base: Url::parse(endpoint.trim_end_matches('/'))?,
            credential: Arc::new(Credential::from_env()),
            part_size: part_size.max(1),
            clock: Clock::system(),
        })
    }

    pub fn with_credential(mut self, credential: Credential) -> AzureStore {
        self.credential = Arc::new(credential);
        self
    }

    pub fn with_part_size(mut self, bytes: usize) -> AzureStore {
        self.part_size = bytes.max(1);
        self
    }

    fn root(&self) -> String {
        self.base.path.trim_end_matches('/').to_owned()
    }

    fn blob_url(&self, key: &str, query: &str) -> Url {
        self.base.with_path(
            &format!(
                "{}/{}/{}",
                self.root(),
                uri_encode(&self.container, false),
                uri_encode(&join(&self.prefix, key), true)
            ),
            query,
        )
    }

    fn container_url(&self, query: &str) -> Url {
        self.base.with_path(
            &format!("{}/{}", self.root(), uri_encode(&self.container, false)),
            query,
        )
    }

    fn call(&self, mut request: Request) -> Result<Response> {
        let now = self.clock.now();
        request = request
            .header("x-ms-version", VERSION)
            .header("x-ms-date", http_date(now));
        match self.credential.as_ref() {
            Credential::SharedKey { key } => {
                let auth = azure::shared_key(&request, &self.account, key)?;
                request = request.header("Authorization", auth);
            }
            Credential::Sas { token } => {
                request.url.query = match request.url.query.is_empty() {
                    true => token.clone(),
                    false => format!("{}&{token}", request.url.query),
                };
            }
            Credential::Identity(identity) => {
                let token = identity.token(now)?;
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            Credential::Anonymous => {}
        }
        send(&request)
    }

    fn put(&self, key: &str, data: &[u8], condition: (&str, &str)) -> Result<Generation> {
        check_key(key)?;
        let response = self.call(
            Request::new("PUT", self.blob_url(key, ""))
                .header("x-ms-blob-type", "BlockBlob")
                .header("Content-Type", "application/octet-stream")
                .header(condition.0, condition.1)
                .body(data.to_vec()),
        )?;
        match response.status {
            200 | 201 => etag(&response),
            // `If-Match` on a blob that is not there.
            404 => Err(Error::precondition(format!("{key} does not exist"))),
            409 | 412 => Err(Error::precondition(format!(
                "{key} is not in the state the write expected"
            ))),
            _ => Err(response.error(&format!("PUT {key}"))),
        }
    }

    fn block_id(key: &str, index: usize) -> String {
        // Every ID of one blob must be the same length.
        let tag = hex::encode(&blake3::hash(key.as_bytes()).as_bytes()[..6]);
        b64(format!("{tag}-{index:08}").as_bytes())
    }
}

fn etag(response: &Response) -> Result<Generation> {
    response
        .header("etag")
        .map(str::to_owned)
        .ok_or_else(|| Error::refused("the response has no ETag"))
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Staged {
    digest: String,
    part_size: usize,
    blocks: usize,
}

impl ObjectStore for AzureStore {
    fn url(&self) -> String {
        let mut url = format!("az://{}/{}", self.account, self.container);
        if !self.prefix.is_empty() {
            url.push('/');
            url.push_str(&self.prefix);
        }
        url
    }

    fn get(&self, key: &str) -> Result<Object> {
        check_key(key)?;
        let response = self.call(Request::new("GET", self.blob_url(key, "")))?;
        match response.status {
            200 => Ok(Object {
                generation: etag(&response)?,
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
            Request::new("GET", self.blob_url(key, ""))
                .header("x-ms-range", format!("bytes={start}-{}", start + len - 1)),
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
        let response = self.call(Request::new("HEAD", self.blob_url(key, "")))?;
        match response.status {
            200 => Ok(Some(Entry {
                key: key.to_owned(),
                size: response
                    .header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
                generation: etag(&response)?,
                modified_ms: response
                    .header("last-modified")
                    .and_then(|t| parse_http_date(t).ok()),
            })),
            404 => Ok(None),
            _ => Err(response.error(&format!("HEAD {key}"))),
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
        let mut marker: Option<String> = None;
        loop {
            let mut query = format!(
                "restype=container&comp=list&prefix={}",
                uri_encode(&full, false)
            );
            if let Some(m) = &marker {
                query.push_str(&format!("&marker={}", uri_encode(m, false)));
            }
            let response = self.call(Request::new("GET", self.container_url(&query)))?;
            if !response.ok() {
                return Err(response.error(&format!("listing {prefix}")));
            }
            let text = response.text();
            for blob in xml::elements(&text, "Blob") {
                let Some(name) = xml::text(blob, "Name") else {
                    continue;
                };
                if name.len() < strip {
                    continue;
                }
                out.push(Entry {
                    key: name[strip..].to_owned(),
                    size: xml::text(blob, "Content-Length")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    generation: xml::text(blob, "Etag").unwrap_or_default(),
                    modified_ms: xml::text(blob, "Last-Modified")
                        .and_then(|t| parse_http_date(&t).ok()),
                });
            }
            match xml::text(&text, "NextMarker") {
                Some(next) if !next.is_empty() => marker = Some(next),
                _ => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()> {
        check_key(key)?;
        let response = self
            .call(Request::new("DELETE", self.blob_url(key, "")).header("If-Match", generation))?;
        match response.status {
            200 | 202 | 204 => Ok(()),
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
        // Staging blocks for a blob that exists would leave them behind
        // for a week; the commit's condition still decides.
        if self.stat(key)?.is_some() {
            return Err(Error::precondition(format!("{key} already exists")));
        }
        let digest = hex::encode(&blake3::hash(data).as_bytes()[..16]);
        let mut staged: Staged = journal
            .load(key)
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|s: &Staged| s.digest == digest && s.part_size == self.part_size)
            .unwrap_or(Staged {
                digest,
                part_size: self.part_size,
                blocks: 0,
            });
        let parts: Vec<&[u8]> = data.chunks(self.part_size).collect();
        for (i, part) in parts.iter().enumerate().skip(staged.blocks) {
            let query = format!(
                "comp=block&blockid={}",
                uri_encode(&AzureStore::block_id(key, i), false)
            );
            let response =
                self.call(Request::new("PUT", self.blob_url(key, &query)).body(part.to_vec()))?;
            if !response.ok() {
                return Err(response.error(&format!("block {i} of {key}")));
            }
            staged.blocks = i + 1;
            journal.save(key, &serde_json::to_string(&staged)?);
        }
        let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for i in 0..parts.len() {
            body.push_str(&format!(
                "<Latest>{}</Latest>",
                AzureStore::block_id(key, i)
            ));
        }
        body.push_str("</BlockList>");
        let response = self.call(
            Request::new("PUT", self.blob_url(key, "comp=blocklist"))
                .header("If-None-Match", "*")
                .header("x-ms-blob-content-type", "application/octet-stream")
                .header("Content-Type", "application/xml")
                .body(body.into_bytes()),
        )?;
        match response.status {
            200 | 201 => {
                journal.clear(key);
                etag(&response)
            }
            409 | 412 => {
                journal.clear(key);
                Err(Error::precondition(format!("{key} already exists")))
            }
            400 if response.text().contains("InvalidBlockList") => {
                // Staged blocks expired: stage them again next time.
                journal.clear(key);
                Err(Error::transient(format!(
                    "the staged blocks of {key} are gone; starting over"
                )))
            }
            _ => Err(response.error(&format!("committing {key}"))),
        }
    }
}
