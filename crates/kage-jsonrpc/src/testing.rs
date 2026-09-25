//! Peers wired back to back for tests.
//!
//! Available under `#[cfg(test)]` automatically, or for downstream crates
//! by enabling the `testing` feature on `kage-jsonrpc`.

use std::io::BufReader;
use std::sync::mpsc::Receiver;

use crate::{CancelNotice, Inbound, Peer, connect, connect_with};

/// One end of a [`pair`]: the peer and its inbound channel.
pub type End = (Peer, Receiver<Inbound>);

/// Two peers connected to each other over OS pipes, as
/// `(client, server)`.
///
/// # Panics
///
/// Panics if the OS cannot create a pipe.
#[must_use]
pub fn pair() -> (End, End) {
    pair_with(None)
}

/// Like [`pair`], with `notice` installed on the client end.
///
/// # Panics
///
/// Panics if the OS cannot create a pipe.
#[must_use]
pub fn pair_with(notice: Option<CancelNotice>) -> (End, End) {
    let (cli_r, srv_w) = std::io::pipe().unwrap();
    let (srv_r, cli_w) = std::io::pipe().unwrap();
    let (cli_peer, cli_in, _) = connect_with(BufReader::new(cli_r), cli_w, notice);
    let (srv_peer, srv_in, _) = connect(BufReader::new(srv_r), srv_w);
    ((cli_peer, cli_in), (srv_peer, srv_in))
}
