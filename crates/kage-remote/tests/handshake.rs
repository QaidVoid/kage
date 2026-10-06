//! Handshake behavior on real sockets: every token presentation, the
//! rejections, and the fields of the 101.

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;

use common::{
    CONNECTION_ID, SAMPLE_ACCEPT, TIMEOUT, fresh_token, listener, serve_upgraded, upgrade_response,
};
use kage_remote::HEAD_CAP;
use kage_remote::head::{TOKEN_SUBPROTOCOL_PREFIX, UI_SUBPROTOCOL_PREFIX};

#[test]
fn bearer_header_gets_101_with_accept_and_connection_id() {
    let (_dir, token) = fresh_token();
    let addr = serve_upgraded(Arc::clone(&token), |_, _| {});
    let reply = upgrade_response(
        addr,
        "/acp",
        &[("Authorization", &format!("Bearer {}", token.as_str()))],
    );
    assert!(
        reply.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "{reply}"
    );
    assert!(reply.contains("\r\nUpgrade: websocket\r\n"), "{reply}");
    assert!(reply.contains("\r\nConnection: Upgrade\r\n"), "{reply}");
    assert!(
        reply.contains(&format!("\r\nSec-WebSocket-Accept: {SAMPLE_ACCEPT}\r\n")),
        "{reply}"
    );
    assert!(
        reply.contains(&format!("\r\nAcp-Connection-Id: {CONNECTION_ID}\r\n")),
        "{reply}"
    );
    assert!(!reply.contains("Sec-WebSocket-Protocol"), "{reply}");
}

#[test]
fn token_subprotocol_is_accepted_and_echoed() {
    let (_dir, token) = fresh_token();
    let addr = serve_upgraded(Arc::clone(&token), |_, _| {});
    let entry = format!("{TOKEN_SUBPROTOCOL_PREFIX}{}", token.as_str());
    let reply = upgrade_response(
        addr,
        "/acp",
        &[("Sec-WebSocket-Protocol", &format!("chat, {entry}"))],
    );
    assert!(reply.starts_with("HTTP/1.1 101"), "{reply}");
    assert!(
        reply.contains(&format!("\r\nSec-WebSocket-Protocol: {entry}\r\n")),
        "{reply}"
    );
}

#[test]
fn ui_subprotocol_form_is_accepted_and_echoed() {
    let (_dir, token) = fresh_token();
    let addr = serve_upgraded(Arc::clone(&token), |_, _| {});
    let entry = format!("{UI_SUBPROTOCOL_PREFIX}{}", token.as_str());
    let reply = upgrade_response(addr, "/acp", &[("Sec-WebSocket-Protocol", &entry)]);
    assert!(reply.starts_with("HTTP/1.1 101"), "{reply}");
    assert!(
        reply.contains(&format!("\r\nSec-WebSocket-Protocol: {entry}\r\n")),
        "{reply}"
    );
}

#[test]
fn query_token_gets_101() {
    let (_dir, token) = fresh_token();
    let addr = serve_upgraded(Arc::clone(&token), |_, _| {});
    let reply = upgrade_response(addr, &format!("/acp?token={}", token.as_str()), &[]);
    assert!(reply.starts_with("HTTP/1.1 101"), "{reply}");
    assert!(!reply.contains("Sec-WebSocket-Protocol"), "{reply}");
}

#[test]
fn missing_and_wrong_tokens_get_401() {
    let (_dir, token) = fresh_token();
    let rejected = |target: &str, headers: &[(&str, &str)]| {
        let addr = serve_upgraded(Arc::clone(&token), |_, _| {});
        upgrade_response(addr, target, headers)
    };

    let plain = rejected("/acp", &[]);
    assert!(plain.starts_with("HTTP/1.1 401"), "{plain}");

    let wrong_bearer = rejected("/acp", &[("Authorization", "Bearer deadbeef")]);
    assert!(wrong_bearer.starts_with("HTTP/1.1 401"), "{wrong_bearer}");

    let wrong_protocol = rejected("/acp", &[("Sec-WebSocket-Protocol", "kage.deadbeef")]);
    assert!(
        wrong_protocol.starts_with("HTTP/1.1 401"),
        "{wrong_protocol}"
    );

    let wrong_query = rejected("/acp?token=deadbeef", &[]);
    assert!(wrong_query.starts_with("HTTP/1.1 401"), "{wrong_query}");
}

#[test]
fn oversized_head_gets_431() {
    let (_dir, token) = fresh_token();
    let (listener, addr) = listener();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = common::accept(&mut stream, &token);
    });
    let mut client = TcpStream::connect(addr).unwrap();
    client.set_read_timeout(Some(TIMEOUT)).unwrap();
    let pad = "x".repeat(HEAD_CAP * 2);
    let request = format!("GET /acp HTTP/1.1\r\nX-Pad: {pad}\r\n");
    let _ = client.write_all(request.as_bytes());
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 431 "), "{reply}");
}

#[test]
fn silent_client_gets_no_answer_until_the_deadline() {
    let (listener, addr) = listener();
    let (done, waited) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let outcome = kage_remote::head::read_head_with_timeout(&mut stream, TIMEOUT);
        let _ = done.send(outcome);
    });
    let _client = TcpStream::connect(addr).unwrap();
    let outcome = waited.recv_timeout(TIMEOUT + TIMEOUT).unwrap();
    assert!(
        matches!(outcome, Err(kage_remote::head::HeadError::Timeout(_))),
        "{outcome:?}"
    );
}
