//! Frame behavior on real sockets: one message per text frame, raw
//! newlines and binary frames, the inbound cap, and the outgoing
//! pressure cap.

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use common::{TIMEOUT, connect_client, fresh_token, serve_upgraded};
use kage_jsonrpc::Inbound;
use kage_remote::MAX_MESSAGE;
use tungstenite::Message;

fn request(id: u64) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping","params":{{}}}}"#)
}

#[test]
fn request_frame_round_trips_through_jsonrpc_connect() {
    let (_dir, token) = fresh_token();
    let addr = serve_upgraded(Arc::clone(&token), |reader, writer| {
        let start = std::time::Instant::now();
        let (peer, inbound, _thread) = kage_jsonrpc::connect(reader, writer);
        match inbound.recv_timeout(TIMEOUT) {
            Ok(Inbound::Request { id, .. }) => {
                eprintln!("[diag] request at {:?}", start.elapsed());
                peer.respond(&id, Ok(serde_json::json!({ "pong": true })))
                    .unwrap();
                eprintln!("[diag] respond done at {:?}", start.elapsed());
            }
            other => panic!("expected a request, got {other:?}"),
        }
        // Hold the connection open like the real serve loop until the
        // client goes away, so the reply is not cut off by an early
        // drop of the socket.
        let _ = inbound.recv();
    });

    let mut client = connect_client(addr, &token);
    client.send(Message::text(request(7))).unwrap();
    let Message::Text(reply) = client.read().unwrap() else {
        panic!("expected a text reply");
    };
    let value: serde_json::Value = serde_json::from_str(reply.as_str()).unwrap();
    assert_eq!(value["id"], 7);
    assert_eq!(value["result"]["pong"], true);
}

#[test]
fn raw_newline_frame_still_arrives_as_one_message() {
    let (_dir, token) = fresh_token();
    let (requests, seen) = mpsc::channel();
    let addr = serve_upgraded(Arc::clone(&token), move |reader, writer| {
        let (_peer, inbound, _thread) = kage_jsonrpc::connect(reader, writer);
        while let Ok(message) = inbound.recv_timeout(TIMEOUT) {
            if requests.send(message).is_err() {
                break;
            }
        }
    });

    let mut client = connect_client(addr, &token);
    let raw = format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"ping",{}"params":{{}}}}"#,
        '\n'
    );
    client.send(Message::text(raw)).unwrap();
    // The client hangs up, so the server drains everything it parsed
    // and exits. A split frame would show up as an extra inbound
    // message or an outbound -32700 before the close.
    let _ = client.send(Message::Close(None));

    let mut requests = Vec::new();
    while let Ok(message) = seen.recv_timeout(TIMEOUT) {
        requests.push(message);
    }
    let [Inbound::Request { id, .. }] = &requests[..] else {
        panic!("exactly one parsed request expected, got {requests:?}");
    };
    assert_eq!(id, &serde_json::json!(9));

    // Drain whatever the server sent before closing: only the close
    // handshake, never a parse error for a split half.
    loop {
        match client.read() {
            Ok(Message::Text(line)) => {
                panic!("the server answered nothing before closing: {line}");
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[test]
fn binary_frames_are_dropped() {
    let (_dir, token) = fresh_token();
    let (requests, seen) = mpsc::channel();
    let addr = serve_upgraded(Arc::clone(&token), move |reader, writer| {
        let (_peer, inbound, _thread) = kage_jsonrpc::connect(reader, writer);
        while let Ok(message) = inbound.recv_timeout(TIMEOUT) {
            if requests.send(message).is_err() {
                break;
            }
        }
    });

    let mut client = connect_client(addr, &token);
    client.send(Message::binary(vec![0, 1, 2, 3])).unwrap();
    client.send(Message::text(request(10))).unwrap();
    // Hang up so the server drains everything it accepted and exits.
    // A forwarded binary frame would appear in `seen` before the
    // close.
    let _ = client.send(Message::Close(None));

    let mut requests = Vec::new();
    while let Ok(message) = seen.recv_timeout(TIMEOUT) {
        requests.push(message);
    }
    let [Inbound::Request { id, .. }] = &requests[..] else {
        panic!("only the text request expected, got {requests:?}");
    };
    assert_eq!(id, &serde_json::json!(10));
}

#[test]
fn message_over_the_cap_closes_the_connection() {
    let (_dir, token) = fresh_token();
    let (gone, is_gone) = mpsc::channel();
    let addr = serve_upgraded(Arc::clone(&token), move |reader, writer| {
        let (_peer, inbound, thread) = kage_jsonrpc::connect(reader, writer);
        drop(inbound);
        thread.join().unwrap();
        let _ = gone.send(());
    });

    let mut client = connect_client(addr, &token);
    // The server may close mid-send once it sees the over-cap frame
    // header; the failed send is the close being observed.
    let _ = client.send(Message::text("a".repeat(MAX_MESSAGE + 1)));
    loop {
        match client.read() {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    is_gone
        .recv_timeout(TIMEOUT)
        .unwrap_or_else(|_| panic!("the server must close on a message over {MAX_MESSAGE} bytes"));
}

#[test]
fn stalled_reader_fails_writes_past_the_cap_and_closes() {
    let (_dir, token) = fresh_token();
    let (done, is_done) = mpsc::channel();
    let addr = serve_upgraded(Arc::clone(&token), move |reader, writer| {
        let (peer, inbound, _thread) = kage_jsonrpc::connect(reader, writer);
        let pad = "x".repeat(1024 * 1024);
        let start = Instant::now();
        let mut failed_at = None;
        for i in 0..4096u32 {
            if peer.notify("n", serde_json::json!({ "pad": pad })).is_err() {
                failed_at = Some(i);
                break;
            }
        }
        let elapsed = start.elapsed();
        drop(peer);
        assert!(
            failed_at.is_some(),
            "writes must fail once the cap is passed"
        );
        assert!(
            elapsed < Duration::from_secs(30),
            "writes must not block: {elapsed:?}"
        );
        // The writer may sit in one final blocked send for up to its
        // write timeout before the connection goes down.
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            match inbound.recv_timeout(Duration::from_secs(1)) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "the connection must close after writes fail"
                    );
                }
                Ok(other) => panic!("expected the connection to close, got {other:?}"),
            }
        }
        let _ = done.send(());
    });

    // A raw client that completes the upgrade and then never reads.
    // It must not touch the socket while the flood runs, or it would
    // drain the server's buffers and nothing would ever stall.
    let mut client = TcpStream::connect(addr).unwrap();
    client.set_read_timeout(Some(TIMEOUT)).unwrap();
    let upgrade = format!(
        "GET /acp?token={} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        token.as_str()
    );
    client.write_all(upgrade.as_bytes()).unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = client.read(&mut byte).unwrap();
        assert!(read > 0, "connection closed during the upgrade");
        head.extend_from_slice(&byte);
    }

    is_done
        .recv_timeout(Duration::from_secs(60))
        .expect("the server must give up on a client that never reads");
    // Once the server gives up on us, the socket must close.
    let mut junk = [0u8; 8192];
    loop {
        match client.read(&mut junk) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[test]
fn frame_bytes_after_the_head_are_not_lost() {
    let (_dir, token) = fresh_token();
    let (requests, seen) = mpsc::channel();
    let addr = serve_upgraded(Arc::clone(&token), move |reader, writer| {
        let (_peer, inbound, _thread) = kage_jsonrpc::connect(reader, writer);
        if let Ok(message) = inbound.recv_timeout(TIMEOUT) {
            let _ = requests.send(message);
        }
    });

    let mut client = TcpStream::connect(addr).unwrap();
    client.set_write_timeout(Some(TIMEOUT)).unwrap();
    let head = format!(
        "GET /acp HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: kage.{}\r\n\r\n",
        token.as_str()
    );
    let mut packet = head.into_bytes();
    packet.extend_from_slice(&masked_text(request(5).as_bytes()));
    client.write_all(&packet).unwrap();

    match seen.recv_timeout(TIMEOUT).unwrap() {
        Inbound::Request { id, .. } => assert_eq!(id, serde_json::json!(5)),
        other @ Inbound::Notification { .. } => {
            panic!("expected the frame pipelined after the head, got {other:?}")
        }
    }
}

/// One masked client text frame, as a browser would send it.
fn masked_text(payload: &[u8]) -> Vec<u8> {
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    let mut frame = vec![0x81u8];
    frame.push(0x80 | u8::try_from(payload.len()).unwrap());
    frame.extend_from_slice(&mask);
    for (i, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[i % 4]);
    }
    frame
}
