//! The in-process cloud mocks read requests through the wire codec, so
//! the corpus every consumer runs applies to them too: a request the
//! codec refuses is answered 400 and never reaches a handler.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;

use branchyard_sync::testing::server::{MockResponse, MockServer};
use branchyard_wire as wire;

fn server() -> MockServer {
    MockServer::start(Arc::new(|request| {
        MockResponse::new(200).body(request.body.clone())
    }))
}

fn send(server: &MockServer, bytes: &[u8]) -> String {
    let mut stream = TcpStream::connect(server.url.trim_start_matches("http://")).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let _ = stream.write_all(bytes);
    let _ = stream.shutdown(Shutdown::Write);
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    String::from_utf8_lossy(&answer).into_owned()
}

#[test]
fn valid_requests_reach_the_handler_with_their_bodies() {
    let server = server();
    for (name, bytes, body) in wire::corpus::requests_valid() {
        let answer = send(&server, &bytes);
        assert!(answer.starts_with("HTTP/1.1 200 "), "{name}: {answer:?}");
        assert!(
            answer.ends_with(&String::from_utf8_lossy(&body).into_owned()),
            "{name}: {answer:?}"
        );
    }
}

#[test]
fn malformed_requests_get_a_400_and_no_handler() {
    let server = server();
    for (name, bytes) in wire::corpus::requests_malformed() {
        let answer = send(&server, &bytes);
        assert!(answer.starts_with("HTTP/1.1 400 "), "{name}: {answer:?}");
    }
    assert!(
        server.requests().is_empty(),
        "a refused request reached a handler: {:?}",
        server.requests()
    );
}
