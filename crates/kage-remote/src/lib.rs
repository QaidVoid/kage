//! Remote transport: the ACP WebSocket profile as a standalone crate.
//!
//! An accepted TCP stream speaking the ACP WebSocket profile becomes
//! the newline-delimited JSON-RPC reader and writer pair the agent
//! serving loop already consumes for stdio. [`head`] parses and
//! authorizes the upgrade request, [`pipe::upgrade`] writes the `101`
//! and runs the frame-to-line plumbing on plain threads, and [`token`]
//! owns the bearer secret guarding the endpoint. No async runtime is
//! involved anywhere.
//!
//! The wire profile: a `GET` upgrade request carries the token in the
//! `Authorization: Bearer` header, in a `Sec-WebSocket-Protocol`
//! entry, or in the `token` query parameter. The `101` response names
//! the connection with `Acp-Connection-Id`. Afterwards every JSON-RPC
//! message is one text frame and every text frame is one message;
//! binary frames are ignored, and `initialize` is expected as the
//! first message.
//!
//! The limits as public constants: [`HEAD_CAP`] and [`HEAD_TIMEOUT`]
//! bound the request head, [`MAX_MESSAGE`] bounds one inbound message,
//! [`OUTGOING_CAP`] bounds the outgoing queue, and [`PING_INTERVAL`]
//! with [`IDLE_TIMEOUT`] govern keepalive and teardown.

use std::time::Duration;

pub mod head;
pub mod pipe;
pub mod token;

/// Largest accepted request head in bytes. A longer head is answered
/// with `431 Request Header Fields Too Large`.
pub const HEAD_CAP: usize = 16 * 1024;

/// How long the peer may take to deliver a complete request head.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest inbound message in bytes. A larger message closes the
/// connection.
pub const MAX_MESSAGE: usize = 8 * 1024 * 1024;

/// Byte cap on messages queued for a peer that stopped reading. Once
/// the cap is passed, writes fail and the connection closes.
pub const OUTGOING_CAP: u64 = 64 * 1024 * 1024;

/// How often the server pings the peer to probe for liveness.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// How long the connection may go without any inbound frame before it
/// is closed as idle.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
