//! Traces of Branchyard's own work, in OpenTelemetry's model: admission,
//! claim, the operation a worker runs, each turn it ran, the tool calls and
//! connector calls inside them. See `docs/observability.md`.
//!
//! The `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` and
//! `tracing-opentelemetry` crates could not be fetched in the offline build
//! this was written in (none is in `Cargo.lock` or the local registry), so
//! this module is the small part of them the server needs: a span model,
//! W3C `traceparent` contexts, a batching [`Tracer`] that hands finished
//! spans to a pluggable [`SpanExporter`], and [`OtlpExporter`], which sends
//! them as OTLP over HTTP, protobuf-encoded (with `prost`, which the
//! workspace already depends on) or JSON-encoded. Live spans also enter a
//! [`tracing`] span carrying the trace and span IDs, so log lines written
//! inside them can be joined to the trace.
//!
//! Turns, tool calls and connector calls happen in the engine and the
//! harness; their spans are made after the operation that ran them
//! finishes, from the events it recorded (with their own timestamps), by
//! [`crate::observe`].
//!
//! Configured from the standard variables: `OTEL_EXPORTER_OTLP_ENDPOINT`
//! (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`), `OTEL_EXPORTER_OTLP_PROTOCOL`
//! (`http/protobuf`, the default, or `http/json`; `grpc` is not built in),
//! `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME` and `OTEL_SDK_DISABLED`.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A span's identity: its trace and its own ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SpanContext {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    let mut filled = 0;
    while filled < N {
        let key = branchyard_client::new_key();
        let bytes = hex::decode(&key).unwrap_or_default();
        for b in bytes {
            if filled == N {
                break;
            }
            out[filled] = b;
            filled += 1;
        }
    }
    out
}

impl SpanContext {
    /// A new trace's first span.
    pub fn root() -> SpanContext {
        SpanContext {
            trace_id: random_bytes(),
            span_id: random_bytes(),
        }
    }

    /// A new span in the same trace.
    pub fn child(&self) -> SpanContext {
        SpanContext {
            trace_id: self.trace_id,
            span_id: random_bytes(),
        }
    }

    /// The W3C `traceparent` header value, sampled.
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-01",
            hex::encode(self.trace_id),
            hex::encode(self.span_id)
        )
    }

    /// A `traceparent` header value (W3C Trace Context, version 00, or a
    /// later version read as 00); `None` when malformed or all zeros.
    pub fn parse(header: &str) -> Option<SpanContext> {
        let header = header.trim();
        let mut parts = header.split('-');
        let (version, trace, span, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        let lower_hex = |s: &str, n: usize| {
            s.len() == n
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !lower_hex(version, 2) || version == "ff" || !lower_hex(flags, 2) {
            return None;
        }
        if version == "00" && parts.next().is_some() {
            return None;
        }
        if !lower_hex(trace, 32) || !lower_hex(span, 16) {
            return None;
        }
        let trace_id: [u8; 16] = hex::decode(trace).ok()?.try_into().ok()?;
        let span_id: [u8; 8] = hex::decode(span).ok()?.try_into().ok()?;
        if trace_id == [0; 16] || span_id == [0; 8] {
            return None;
        }
        Some(SpanContext { trace_id, span_id })
    }
}

/// What a span measures, as OTLP numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Internal = 1,
    Server = 2,
    Client = 3,
}

/// An attribute's value.
#[derive(Clone, Debug, PartialEq)]
pub enum Attr {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl From<&str> for Attr {
    fn from(value: &str) -> Attr {
        Attr::Str(value.to_owned())
    }
}
impl From<String> for Attr {
    fn from(value: String) -> Attr {
        Attr::Str(value)
    }
}
impl From<i64> for Attr {
    fn from(value: i64) -> Attr {
        Attr::Int(value)
    }
}
impl From<f64> for Attr {
    fn from(value: f64) -> Attr {
        Attr::Float(value)
    }
}
impl From<bool> for Attr {
    fn from(value: bool) -> Attr {
        Attr::Bool(value)
    }
}

/// A finished span.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanData {
    pub name: String,
    pub context: SpanContext,
    pub parent: Option<[u8; 8]>,
    pub kind: SpanKind,
    pub start_ns: u64,
    pub end_ns: u64,
    pub attributes: Vec<(String, Attr)>,
    /// Set when the span ended in error, with what went wrong.
    pub error: Option<String>,
}

impl SpanData {
    /// A span of `context`, child of `parent`, from `start_ms` to `end_ms`.
    pub fn new(
        name: impl Into<String>,
        context: SpanContext,
        parent: Option<&SpanContext>,
        start_ms: u64,
        end_ms: u64,
    ) -> SpanData {
        SpanData {
            name: name.into(),
            context,
            parent: parent.map(|p| p.span_id),
            kind: SpanKind::Internal,
            start_ns: start_ms.saturating_mul(1_000_000),
            end_ns: end_ms.max(start_ms).saturating_mul(1_000_000),
            attributes: Vec::new(),
            error: None,
        }
    }

    pub fn attr(mut self, key: &str, value: impl Into<Attr>) -> SpanData {
        self.attributes.push((key.to_owned(), value.into()));
        self
    }

    pub fn attribute(&self, key: &str) -> Option<&Attr> {
        self.attributes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

/// Where finished spans go. Called from the tracer's own thread with a
/// batch at a time; an error is logged and the batch dropped.
pub trait SpanExporter: Send + Sync {
    fn export(&self, batch: Vec<SpanData>) -> Result<(), String>;
}

/// Keeps every span it is given: for tests, and for embedding.
#[derive(Default)]
pub struct MemoryExporter {
    spans: Mutex<Vec<SpanData>>,
}

impl MemoryExporter {
    pub fn spans(&self) -> Vec<SpanData> {
        self.spans.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl SpanExporter for MemoryExporter {
    fn export(&self, batch: Vec<SpanData>) -> Result<(), String> {
        self.spans
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend(batch);
        Ok(())
    }
}

enum Message {
    Span(Box<SpanData>),
    Flush(mpsc::Sender<()>),
}

/// Spans exported per batch, at most.
const BATCH: usize = 256;
/// How long a span waits in a partial batch, at most.
const FLUSH_EVERY: Duration = Duration::from_secs(2);
/// Branches whose latest trace is remembered for webhook deliveries.
const REMEMBERED: usize = 4096;

struct Inner {
    sender: Option<Mutex<mpsc::Sender<Message>>>,
    /// The trace each branch's latest operation ran in, by
    /// `repo/branch`, for the `traceparent` of webhook deliveries.
    branches: Mutex<HashMap<String, String>>,
}

/// Records finished spans on a thread of its own, batched, to its
/// exporter; or nothing, when it has none. Cheap to clone.
#[derive(Clone)]
pub struct Tracer(Arc<Inner>);

impl std::fmt::Debug for Tracer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracer")
            .field("exporting", &self.enabled())
            .finish()
    }
}

impl Default for Tracer {
    fn default() -> Tracer {
        Tracer::disabled()
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Tracer {
    /// Records nothing; contexts still propagate.
    pub fn disabled() -> Tracer {
        Tracer(Arc::new(Inner {
            sender: None,
            branches: Mutex::new(HashMap::new()),
        }))
    }

    /// Hand finished spans to `exporter`, in batches, on a thread of its
    /// own.
    pub fn new(exporter: Arc<dyn SpanExporter>) -> Tracer {
        let (sender, receiver) = mpsc::channel::<Message>();
        let spawned = std::thread::Builder::new()
            .name("branchyard-traces".into())
            .spawn(move || export_loop(receiver, exporter));
        if let Err(e) = spawned {
            tracing::error!(error = %e, "could not start the trace exporter; traces are off");
            return Tracer::disabled();
        }
        Tracer(Arc::new(Inner {
            sender: Some(Mutex::new(sender)),
            branches: Mutex::new(HashMap::new()),
        }))
    }

    /// From the OpenTelemetry variables: an OTLP exporter when
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` (or `..._TRACES_ENDPOINT`) is set,
    /// otherwise none. A warning explains a setting it cannot honour.
    pub fn from_env() -> Tracer {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        match OtlpExporter::from_vars(env) {
            Ok(None) => Tracer::disabled(),
            Ok(Some(exporter)) => {
                tracing::info!(
                    endpoint = %exporter.endpoint,
                    protocol = exporter.protocol.name(),
                    "exporting traces over OTLP"
                );
                Tracer::new(Arc::new(exporter))
            }
            Err(e) => {
                tracing::warn!("traces are off: {e}");
                Tracer::disabled()
            }
        }
    }

    /// Whether finished spans go anywhere.
    pub fn enabled(&self) -> bool {
        self.0.sender.is_some()
    }

    /// Record a finished span.
    pub fn record(&self, span: SpanData) {
        if let Some(sender) = &self.0.sender {
            let sender = sender.lock().unwrap_or_else(|p| p.into_inner());
            let _ = sender.send(Message::Span(Box::new(span)));
        }
    }

    /// Wait until every span recorded so far has been handed to the
    /// exporter, for at most `timeout`.
    pub fn flush(&self, timeout: Duration) {
        let Some(sender) = &self.0.sender else {
            return;
        };
        let (done, wait) = mpsc::channel();
        let sent = sender
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(Message::Flush(done));
        if sent.is_ok() {
            let _ = wait.recv_timeout(timeout);
        }
    }

    /// The context a new trace or a continued one starts from: `incoming`
    /// when it is a valid `traceparent`, else a new root when this tracer
    /// exports; `None` otherwise (nothing to propagate).
    pub fn start_context(
        &self,
        incoming: Option<&str>,
    ) -> Option<(SpanContext, Option<SpanContext>)> {
        match incoming.and_then(SpanContext::parse) {
            Some(parent) => Some((parent.child(), Some(parent))),
            None if self.enabled() => Some((SpanContext::root(), None)),
            None => None,
        }
    }

    /// A span measured from now until [`Active::end`] (or its drop), which
    /// also enters a [`tracing`] span carrying its IDs.
    pub fn start(&self, name: &str, context: SpanContext, parent: Option<&SpanContext>) -> Active {
        let span = tracing::info_span!(
            "span",
            otel.name = name,
            trace_id = %hex::encode(context.trace_id),
            span_id = %hex::encode(context.span_id),
        );
        Active {
            tracer: self.clone(),
            data: Some(SpanData::new(name, context, parent, now_ms(), now_ms())),
            _span: span,
        }
    }

    /// Remember that `repo`'s `branch` is being worked on in `traceparent`'s
    /// trace, for webhook deliveries of its activity.
    pub fn note_branch(&self, repo: &str, branch: &str, traceparent: &str) {
        let mut branches = self.0.branches.lock().unwrap_or_else(|p| p.into_inner());
        if branches.len() >= REMEMBERED {
            branches.clear();
        }
        branches.insert(format!("{repo}/{branch}"), traceparent.to_owned());
    }

    /// The trace `repo`'s `branch` was last worked on in, here.
    pub fn branch_trace(&self, repo: &str, branch: &str) -> Option<String> {
        self.0
            .branches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&format!("{repo}/{branch}"))
            .cloned()
    }
}

/// A span in progress; recorded when ended or dropped.
pub struct Active {
    tracer: Tracer,
    data: Option<SpanData>,
    _span: tracing::Span,
}

impl Active {
    pub fn context(&self) -> SpanContext {
        self.data
            .as_ref()
            .expect("ended spans are consumed")
            .context
    }

    pub fn set(&mut self, key: &str, value: impl Into<Attr>) {
        if let Some(data) = &mut self.data {
            data.attributes.push((key.to_owned(), value.into()));
        }
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        if let Some(data) = &mut self.data {
            data.error = Some(message.into());
        }
    }

    pub fn kind(&mut self, kind: SpanKind) {
        if let Some(data) = &mut self.data {
            data.kind = kind;
        }
    }

    pub fn end(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(mut data) = self.data.take() {
            data.end_ns = (now_ms() * 1_000_000).max(data.start_ns);
            self.tracer.record(data);
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.finish();
    }
}

fn export_loop(receiver: mpsc::Receiver<Message>, exporter: Arc<dyn SpanExporter>) {
    let mut batch: Vec<SpanData> = Vec::new();
    let send = |batch: &mut Vec<SpanData>| {
        if batch.is_empty() {
            return;
        }
        let spans = std::mem::take(batch);
        let n = spans.len();
        // An exporter that panics loses its batch, not the tracer.
        let exported =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exporter.export(spans)))
                .unwrap_or_else(|_| Err("the exporter panicked".into()));
        if let Err(e) = exported {
            tracing::warn!(spans = n, error = %e, "could not export traces; dropped them");
        }
    };
    loop {
        match receiver.recv_timeout(FLUSH_EVERY) {
            Ok(Message::Span(span)) => {
                batch.push(*span);
                if batch.len() >= BATCH {
                    send(&mut batch);
                }
            }
            Ok(Message::Flush(done)) => {
                send(&mut batch);
                let _ = done.send(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => send(&mut batch),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                send(&mut batch);
                return;
            }
        }
    }
}

/// How spans are encoded on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// `application/x-protobuf`: the OTLP/HTTP default.
    Protobuf,
    /// `application/json`, OTLP's JSON mapping.
    Json,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Protobuf => "http/protobuf",
            Protocol::Json => "http/json",
        }
    }
}

/// Sends spans to an OpenTelemetry collector over OTLP/HTTP.
pub struct OtlpExporter {
    pub endpoint: String,
    pub protocol: Protocol,
    pub headers: Vec<(String, String)>,
    pub service: String,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

/// How long one export may take.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

impl OtlpExporter {
    /// From the OpenTelemetry variables, read through `var`; `None` when no
    /// endpoint is set or `OTEL_SDK_DISABLED` is `true`.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Option<OtlpExporter>, String> {
        if var("OTEL_SDK_DISABLED").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
            return Ok(None);
        }
        if var("OTEL_TRACES_EXPORTER").is_some_and(|v| v.trim() == "none") {
            return Ok(None);
        }
        let endpoint = match (
            var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"),
            var("OTEL_EXPORTER_OTLP_ENDPOINT"),
        ) {
            (Some(url), _) => url.trim().to_owned(),
            (None, Some(base)) => format!("{}/v1/traces", base.trim().trim_end_matches('/')),
            (None, None) => return Ok(None),
        };
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return Err(format!(
                "OTLP endpoint {endpoint:?} is not an http:// or https:// URL"
            ));
        }
        let protocol = match var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
            .or_else(|| var("OTEL_EXPORTER_OTLP_PROTOCOL"))
            .as_deref()
            .map(str::trim)
        {
            None | Some("http/protobuf") => Protocol::Protobuf,
            Some("http/json") => Protocol::Json,
            Some("grpc") => {
                return Err("OTLP over gRPC is not built into this server; set \
                     OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf (or http/json) and point the \
                     endpoint at the collector's HTTP port (4318)"
                    .into())
            }
            Some(other) => return Err(format!("unknown OTLP protocol {other:?}")),
        };
        let headers = parse_headers(
            &var("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
                .or_else(|| var("OTEL_EXPORTER_OTLP_HEADERS"))
                .unwrap_or_default(),
        )?;
        let service = var("OTEL_SERVICE_NAME").unwrap_or_else(|| "branchyard-server".into());
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(EXPORT_TIMEOUT)
            .build()
            .map_err(|e| format!("OTLP client: {e}"))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("OTLP exporter runtime: {e}"))?;
        Ok(Some(OtlpExporter {
            endpoint,
            protocol,
            headers,
            service,
            client,
            runtime,
        }))
    }

    /// `batch` as this exporter's protocol encodes it, with its content
    /// type.
    pub fn encode(&self, batch: &[SpanData]) -> (Vec<u8>, &'static str) {
        match self.protocol {
            Protocol::Protobuf => (
                otlp::encode_protobuf(&self.service, batch),
                "application/x-protobuf",
            ),
            Protocol::Json => (
                otlp::encode_json(&self.service, batch).into_bytes(),
                "application/json",
            ),
        }
    }
}

impl SpanExporter for OtlpExporter {
    fn export(&self, batch: Vec<SpanData>) -> Result<(), String> {
        let (body, content_type) = self.encode(&batch);
        // Built and sent inside the runtime: sending arms its timeout on
        // the runtime's timer.
        let response = self
            .runtime
            .block_on(async {
                let mut request = self
                    .client
                    .post(&self.endpoint)
                    .header("content-type", content_type)
                    .body(body);
                for (name, value) in &self.headers {
                    request = request.header(name.as_str(), value.as_str());
                }
                request.send().await
            })
            .map_err(|e| format!("{}: {e}", self.endpoint))?;
        match response.status().is_success() {
            true => Ok(()),
            false => Err(format!("{} answered {}", self.endpoint, response.status())),
        }
    }
}

/// `OTEL_EXPORTER_OTLP_HEADERS`: `key=value` pairs, comma-separated, each
/// percent-decoded.
pub fn parse_headers(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut headers = Vec::new();
    for pair in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("OTLP header {pair:?} is not key=value"))?;
        headers.push((percent_decode(key.trim()), percent_decode(value.trim())));
    }
    Ok(headers)
}

/// `%XX` escapes; everything else, `+` included, as it is.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let pair = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = pair.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// OTLP's trace messages (opentelemetry-proto, `trace/v1` and its
/// dependencies), the fields Branchyard writes, with their numbers.
pub mod otlp {
    use super::{Attr, SpanData};

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ExportTraceServiceRequest {
        #[prost(message, repeated, tag = "1")]
        pub resource_spans: Vec<ResourceSpans>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ResourceSpans {
        #[prost(message, optional, tag = "1")]
        pub resource: Option<Resource>,
        #[prost(message, repeated, tag = "2")]
        pub scope_spans: Vec<ScopeSpans>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Resource {
        #[prost(message, repeated, tag = "1")]
        pub attributes: Vec<KeyValue>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ScopeSpans {
        #[prost(message, optional, tag = "1")]
        pub scope: Option<InstrumentationScope>,
        #[prost(message, repeated, tag = "2")]
        pub spans: Vec<Span>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct InstrumentationScope {
        #[prost(string, tag = "1")]
        pub name: String,
        #[prost(string, tag = "2")]
        pub version: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Span {
        #[prost(bytes = "vec", tag = "1")]
        pub trace_id: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        pub span_id: Vec<u8>,
        #[prost(bytes = "vec", tag = "4")]
        pub parent_span_id: Vec<u8>,
        #[prost(string, tag = "5")]
        pub name: String,
        #[prost(int32, tag = "6")]
        pub kind: i32,
        #[prost(fixed64, tag = "7")]
        pub start_time_unix_nano: u64,
        #[prost(fixed64, tag = "8")]
        pub end_time_unix_nano: u64,
        #[prost(message, repeated, tag = "9")]
        pub attributes: Vec<KeyValue>,
        #[prost(message, optional, tag = "15")]
        pub status: Option<Status>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Status {
        #[prost(string, tag = "2")]
        pub message: String,
        /// 0 unset, 1 ok, 2 error.
        #[prost(int32, tag = "3")]
        pub code: i32,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct KeyValue {
        #[prost(string, tag = "1")]
        pub key: String,
        #[prost(message, optional, tag = "2")]
        pub value: Option<AnyValue>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct AnyValue {
        #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4")]
        pub value: Option<any_value::Value>,
    }

    pub mod any_value {
        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Value {
            #[prost(string, tag = "1")]
            StringValue(String),
            #[prost(bool, tag = "2")]
            BoolValue(bool),
            #[prost(int64, tag = "3")]
            IntValue(i64),
            #[prost(double, tag = "4")]
            DoubleValue(f64),
        }
    }

    fn value(attr: &Attr) -> AnyValue {
        AnyValue {
            value: Some(match attr {
                Attr::Str(s) => any_value::Value::StringValue(s.clone()),
                Attr::Int(n) => any_value::Value::IntValue(*n),
                Attr::Float(f) => any_value::Value::DoubleValue(*f),
                Attr::Bool(b) => any_value::Value::BoolValue(*b),
            }),
        }
    }

    fn key_value(key: &str, attr: &Attr) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(value(attr)),
        }
    }

    /// One request carrying `batch` under a resource naming `service`.
    pub fn request(service: &str, batch: &[SpanData]) -> ExportTraceServiceRequest {
        let spans = batch
            .iter()
            .map(|s| Span {
                trace_id: s.context.trace_id.to_vec(),
                span_id: s.context.span_id.to_vec(),
                parent_span_id: s.parent.map(|p| p.to_vec()).unwrap_or_default(),
                name: s.name.clone(),
                kind: s.kind as i32,
                start_time_unix_nano: s.start_ns,
                end_time_unix_nano: s.end_ns,
                attributes: s.attributes.iter().map(|(k, v)| key_value(k, v)).collect(),
                status: Some(match &s.error {
                    Some(message) => Status {
                        message: message.clone(),
                        code: 2,
                    },
                    None => Status {
                        message: String::new(),
                        code: 0,
                    },
                }),
            })
            .collect();
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![
                        key_value("service.name", &Attr::Str(service.to_owned())),
                        key_value(
                            "service.version",
                            &Attr::Str(env!("CARGO_PKG_VERSION").to_owned()),
                        ),
                    ],
                }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "branchyard".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    spans,
                }],
            }],
        }
    }

    pub fn encode_protobuf(service: &str, batch: &[SpanData]) -> Vec<u8> {
        prost::Message::encode_to_vec(&request(service, batch))
    }

    fn json_value(attr: &Attr) -> serde_json::Value {
        use serde_json::json;
        match attr {
            Attr::Str(s) => json!({ "stringValue": s }),
            // 64-bit integers are strings in OTLP's JSON mapping.
            Attr::Int(n) => json!({ "intValue": n.to_string() }),
            Attr::Float(f) => json!({ "doubleValue": f }),
            Attr::Bool(b) => json!({ "boolValue": b }),
        }
    }

    /// OTLP's JSON mapping: camelCase fields, trace and span IDs as hex,
    /// 64-bit numbers as strings, enums as numbers.
    pub fn encode_json(service: &str, batch: &[SpanData]) -> String {
        use serde_json::json;
        let kv = |k: &str, v: &Attr| json!({ "key": k, "value": json_value(v) });
        let spans: Vec<serde_json::Value> = batch
            .iter()
            .map(|s| {
                let mut span = json!({
                    "traceId": hex::encode(s.context.trace_id),
                    "spanId": hex::encode(s.context.span_id),
                    "name": s.name,
                    "kind": s.kind as i32,
                    "startTimeUnixNano": s.start_ns.to_string(),
                    "endTimeUnixNano": s.end_ns.to_string(),
                    "attributes": s.attributes.iter().map(|(k, v)| kv(k, v)).collect::<Vec<_>>(),
                    "status": match &s.error {
                        Some(message) => json!({ "code": 2, "message": message }),
                        None => json!({}),
                    },
                });
                if let Some(parent) = s.parent {
                    span["parentSpanId"] = json!(hex::encode(parent));
                }
                span
            })
            .collect();
        json!({
            "resourceSpans": [{
                "resource": { "attributes": [
                    kv("service.name", &Attr::Str(service.to_owned())),
                    kv("service.version", &Attr::Str(env!("CARGO_PKG_VERSION").to_owned())),
                ]},
                "scopeSpans": [{
                    "scope": { "name": "branchyard", "version": env!("CARGO_PKG_VERSION") },
                    "spans": spans,
                }],
            }]
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_round_trips_and_malformed_headers_are_refused() {
        let root = SpanContext::root();
        let child = root.child();
        assert_eq!(child.trace_id, root.trace_id);
        assert_ne!(child.span_id, root.span_id);
        let header = child.traceparent();
        assert_eq!(header.len(), 55);
        assert_eq!(SpanContext::parse(&header), Some(child));
        let good = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let parsed = SpanContext::parse(good).unwrap();
        assert_eq!(
            hex::encode(parsed.trace_id),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(hex::encode(parsed.span_id), "00f067aa0ba902b7");
        // A later version may carry more fields.
        assert!(SpanContext::parse(&format!("01{}-extra", &good[2..])).is_some());
        for bad in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
        ] {
            assert_eq!(SpanContext::parse(bad), None, "{bad:?}");
        }
    }

    fn span() -> SpanData {
        let parent =
            SpanContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let mut span = SpanData::new("turn", parent.child(), Some(&parent), 1_000, 3_500)
            .attr("by.harness", "codex")
            .attr("by.turn", 2i64)
            .attr("by.cost_usd", 0.5)
            .attr("by.retried", true);
        span.error = Some("failed: boom".into());
        span
    }

    #[test]
    fn spans_encode_as_otlp_protobuf_with_its_field_numbers() {
        let span = span();
        let bytes = otlp::encode_protobuf("svc", std::slice::from_ref(&span));
        let decoded: otlp::ExportTraceServiceRequest =
            prost::Message::decode(bytes.as_slice()).unwrap();
        let resource = &decoded.resource_spans[0];
        let service = &resource.resource.as_ref().unwrap().attributes[0];
        assert_eq!(service.key, "service.name");
        let wire = &resource.scope_spans[0].spans[0];
        assert_eq!(wire.trace_id, span.context.trace_id);
        assert_eq!(wire.parent_span_id, span.parent.unwrap());
        assert_eq!(wire.start_time_unix_nano, 1_000_000_000);
        assert_eq!(wire.end_time_unix_nano, 3_500_000_000);
        assert_eq!(wire.status.as_ref().unwrap().code, 2);
        assert_eq!(wire.attributes.len(), 4);
        // The field numbers themselves, independent of the derive: the
        // span's encoding starts with field 1 (trace_id, 16 bytes), then 2
        // (span_id, 8), then 4 (parent, 8), 5 (name), 6 (kind), 7 and 8
        // (fixed64 times).
        let one = prost::Message::encode_to_vec(wire);
        assert_eq!(&one[..2], &[0x0a, 16]);
        assert_eq!(&one[18..20], &[0x12, 8]);
        assert_eq!(&one[28..30], &[0x22, 8]);
        assert_eq!(&one[38..40], &[0x2a, 4]);
        assert_eq!(&one[40..44], b"turn");
        assert_eq!(&one[44..46], &[0x30, 1]);
        assert_eq!(one[46], 0x39);
        assert_eq!(
            u64::from_le_bytes(one[47..55].try_into().unwrap()),
            1_000_000_000
        );
        assert_eq!(one[55], 0x41);
    }

    #[test]
    fn spans_encode_in_otlps_json_mapping() {
        let span = span();
        let text = otlp::encode_json("svc", std::slice::from_ref(&span));
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let wire = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(wire["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(wire["parentSpanId"], "00f067aa0ba902b7");
        assert_eq!(wire["startTimeUnixNano"], "1000000000");
        assert_eq!(wire["kind"], 1);
        assert_eq!(wire["status"]["code"], 2);
        assert_eq!(wire["attributes"][1]["value"]["intValue"], "2");
        assert_eq!(
            json["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "svc"
        );
    }

    #[test]
    fn the_exporter_is_configured_from_the_standard_variables() {
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(OtlpExporter::from_vars(vars(&[])).unwrap().is_none());
        let base = OtlpExporter::from_vars(vars(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318/"),
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                "x-api-key=a%20b+c, tenant = t",
            ),
            ("OTEL_SERVICE_NAME", "by-prod"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(base.endpoint, "http://collector:4318/v1/traces");
        assert_eq!(base.protocol, Protocol::Protobuf);
        assert_eq!(
            base.headers,
            [
                ("x-api-key".to_owned(), "a b+c".to_owned()),
                ("tenant".to_owned(), "t".to_owned())
            ]
        );
        assert_eq!(base.service, "by-prod");
        let traces = OtlpExporter::from_vars(vars(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://ignored:4318"),
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "https://c/v1/t"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(traces.endpoint, "https://c/v1/t");
        assert_eq!(traces.protocol, Protocol::Json);
        let grpc = OtlpExporter::from_vars(vars(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4317"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ]));
        assert!(grpc.err().unwrap().contains("gRPC"));
        assert!(OtlpExporter::from_vars(vars(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            ("OTEL_SDK_DISABLED", "true"),
        ]))
        .unwrap()
        .is_none());
        assert!(
            OtlpExporter::from_vars(vars(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "c:4318")])).is_err()
        );
    }

    #[test]
    fn the_tracer_batches_to_its_exporter_and_remembers_branches() {
        let memory = Arc::new(MemoryExporter::default());
        let tracer = Tracer::new(memory.clone());
        assert!(tracer.enabled());
        let (context, parent) = tracer.start_context(None).unwrap();
        assert!(parent.is_none());
        let mut active = tracer.start("admission", context, None);
        active.set("by.repo", "app");
        active.end();
        tracer.record(span());
        tracer.flush(Duration::from_secs(10));
        let spans = memory.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "admission");
        assert_eq!(
            spans[0].attribute("by.repo"),
            Some(&Attr::Str("app".into()))
        );
        assert!(spans[0].end_ns >= spans[0].start_ns);
        // A disabled tracer still continues an incoming trace.
        let off = Tracer::disabled();
        assert!(off.start_context(None).is_none());
        let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (child, parent) = off.start_context(Some(incoming)).unwrap();
        assert_eq!(parent.unwrap().span_id, child_parent(incoming));
        assert_eq!(hex::encode(child.trace_id), &incoming[3..35]);
        off.note_branch("app", "b", incoming);
        assert_eq!(off.branch_trace("app", "b").as_deref(), Some(incoming));
        assert_eq!(off.branch_trace("app", "c"), None);
    }

    /// A stand-in collector on loopback: answers 200 to each request and
    /// hands back its request line, headers and body.
    /// A request the stand-in collector received: its request line,
    /// headers and body.
    type Received = (String, Vec<(String, String)>, Vec<u8>);

    fn collector() -> (String, mpsc::Receiver<Received>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut headers = Vec::new();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (k, v) = header.split_once(':').unwrap();
                    let (k, v) = (k.trim().to_lowercase(), v.trim().to_owned());
                    if k == "content-length" {
                        length = v.parse().unwrap();
                    }
                    headers.push((k, v));
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
                let _ = send.send((line.trim_end().to_owned(), headers, body));
            }
        });
        (url, receive)
    }

    #[test]
    fn the_otlp_exporter_posts_batches_to_the_collector() {
        for protocol in ["http/protobuf", "http/json"] {
            let (url, received) = collector();
            let exporter = OtlpExporter::from_vars(|name| match name {
                "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(url.clone()),
                "OTEL_EXPORTER_OTLP_PROTOCOL" => Some(protocol.to_owned()),
                "OTEL_EXPORTER_OTLP_HEADERS" => Some("authorization=Bearer%20k".to_owned()),
                _ => None,
            })
            .unwrap()
            .unwrap();
            let tracer = Tracer::new(Arc::new(exporter));
            tracer.record(span());
            tracer.flush(Duration::from_secs(30));
            let (line, headers, body) = received.recv_timeout(Duration::from_secs(30)).unwrap();
            assert_eq!(line, "POST /v1/traces HTTP/1.1");
            let header = |k: &str| {
                headers
                    .iter()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(header("authorization"), Some("Bearer k"));
            match protocol {
                "http/protobuf" => {
                    assert_eq!(header("content-type"), Some("application/x-protobuf"));
                    let request: otlp::ExportTraceServiceRequest =
                        prost::Message::decode(body.as_slice()).unwrap();
                    assert_eq!(
                        request.resource_spans[0].scope_spans[0].spans[0].name,
                        "turn"
                    );
                }
                _ => {
                    assert_eq!(header("content-type"), Some("application/json"));
                    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["name"],
                        "turn"
                    );
                }
            }
        }
    }

    fn child_parent(header: &str) -> [u8; 8] {
        SpanContext::parse(header).unwrap().span_id
    }
}
