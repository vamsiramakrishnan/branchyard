//! The one HTTP/1.1 wire codec in the workspace.
//!
//! Branchyard speaks HTTP/1.1 by hand in a few places: the model
//! gateway's request reader, its client toward a backend, the remote SDK's
//! client (which the sync and CLI crates reuse) and the in-process cloud
//! mocks. They share this crate, so a rule about what is malformed is
//! written once and tested once, against [`corpus`]:
//!
//! - heads: [`read_head`], [`parse_request_head`], [`parse_response_head`]
//!   and the framing rules [`request_framing`] and [`response_framing`],
//!   which refuse `Content-Length` with `Transfer-Encoding`, duplicate or
//!   non-decimal `Content-Length`, and a `Transfer-Encoding` that cannot
//!   be framed;
//! - bodies: [`ChunkedReader`] (the only chunk-size parser; hex digits
//!   only, capped, CRLF checked, truncation an error), [`LengthReader`]
//!   and [`Body`], and [`read_body`] with a limit;
//! - writing: [`request_head`] and [`write_chunk`];
//! - [`HttpUrl`], the one `http(s)` URL.
//!
//! Nothing here defaults on malformed input: the answer is a typed
//! [`WireError`]. Do not parse HTTP framing anywhere else; see
//! `CONTRIBUTING.md` and `tools/check_wire.py`.
#![warn(missing_docs)]

mod body;
mod build;
pub mod corpus;
mod error;
mod head;
mod url;

pub use body::{
    is_token_byte, parse_chunk_size, read_body, write_chunk, Body, ChunkedReader, LengthReader,
    LAST_CHUNK, MAX_CHUNK,
};
pub use build::request_head;
pub use error::{wire_error, WireError};
pub use head::{
    header, parse_request_head, parse_response_head, read_head, read_request_head,
    read_response_head, request_framing, response_framing, Framing, Headers, RequestHead,
    ResponseHead,
};
pub use url::{host_header, HttpUrl};
