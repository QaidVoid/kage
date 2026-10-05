//! The browser transport: a web-sys `WebSocket` carrying ACP frames.
//!
//! The token rides the `kage.<token>` subprotocol entry, never a query
//! string, and the connect states of [`State`] drive the same status
//! bar as the desktop transports. Two facts of the browser platform
//! shape this module, both recorded in `gui/SPIKE.md`:
//!
//! - A failed handshake surfaces only as an `error` plus a `close`
//!   with code 1006; the HTTP status is invisible to JavaScript. A
//!   wrong token and an unreachable endpoint look identical, so both
//!   are retried on the backoff ladder and end in
//!   [`State::Reconnecting`] rather than [`State::Refused`].
//! - Response headers of the 101 are unreadable, so the
//!   `Acp-Connection-Id` cannot be learned here and
//!   [`Transport::connection_id`] always reports none.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{MessageEvent, WebSocket};

use kage_client::Frame;

use super::{Backoff, Event, EventSender, Link, State, Transport};

/// The prefix of the subprotocol entry kage's own client sends.
const TOKEN_SUBPROTOCOL_PREFIX: &str = "kage.";

/// What the dial loop shares across links.
struct Inner {
    events: Option<EventSender>,
    /// The live link, absent between links.
    socket: Option<WebSocket>,
    /// Frames handed over while no link was live, oldest first,
    /// flushed when the next link opens.
    backlog: Vec<Frame>,
    /// Set by [`Transport::close`]; ends every retry.
    closed: bool,
    /// The backoff ladder, shared across retries of one transport.
    backoff: Backoff,
    /// Which retry this is, counting from one per lost link.
    attempt: u32,
    /// The browser callbacks of the live link, dropped with it.
    keepalive: Vec<Closure<dyn FnMut(JsValue)>>,
}

/// How many frames the backlog keeps while no link is live. Beyond
/// this the oldest frame is dropped and reported.
const BACKLOG_CAP: usize = 64;

/// The browser transport to one `kage serve` endpoint. Dropping it
/// closes the link.
pub struct WebTransport {
    dialer: Dialer,
}

impl WebTransport {
    /// A transport for the `ws://` or `wss://` endpoint at `url`,
    /// authenticating with `token` through the subprotocol.
    #[must_use]
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            dialer: Dialer {
                url: url.into(),
                token: token.into(),
                inner: Rc::new(RefCell::new(Inner {
                    events: None,
                    socket: None,
                    backlog: Vec::new(),
                    closed: false,
                    backoff: Backoff::new(),
                    attempt: 0,
                    keepalive: Vec::new(),
                })),
            },
        }
    }
}

/// What the browser callbacks and the retry timer hold: the endpoint
/// and the shared state. Unlike [`WebTransport`], dropping one leaves
/// the link alone.
#[derive(Clone)]
struct Dialer {
    url: String,
    token: String,
    inner: Rc<RefCell<Inner>>,
}

impl Dialer {
    /// Reports one state move into the event stream, best effort: the
    /// receiver is gone once the shell closed.
    fn report(inner: &Inner, state: State) {
        if let Some(events) = &inner.events {
            let _ = events.try_send(Event::State(state));
        }
    }

    /// [`Self::report`] for a transport that is not borrowed yet.
    fn report_borrowed(transport: &Self, state: State) {
        let inner = transport.inner.borrow();
        Self::report(&inner, state);
    }

    /// Dials once and wires the browser callbacks. A drop schedules
    /// another dial on the backoff ladder until
    /// [`Transport::close`]; only a rejected URL gives up at once.
    fn dial(&self) {
        let (events_ready, already_closed) = {
            let inner = self.inner.borrow();
            (inner.events.is_some(), inner.closed)
        };
        if already_closed || !events_ready {
            return;
        }
        Self::report_borrowed(self, State::Connecting);
        let entry = format!("{TOKEN_SUBPROTOCOL_PREFIX}{token}", token = self.token);
        let socket = match WebSocket::new_with_str(&self.url, &entry) {
            Ok(socket) => socket,
            Err(_) => {
                let refused = {
                    let mut inner = self.inner.borrow_mut();
                    let refused = !inner.closed;
                    inner.closed = true;
                    refused
                };
                if refused {
                    Self::report_borrowed(
                        self,
                        State::Refused("the endpoint URL is not a WebSocket URL".to_owned()),
                    );
                    Self::report_borrowed(self, State::Closed);
                }
                return;
            }
        };
        socket.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let on_open = self.bind(|transport, _| {
            let mut inner = transport.inner.borrow_mut();
            inner.backoff.reset();
            inner.attempt = 0;
            for frame in std::mem::take(&mut inner.backlog) {
                if let Some(socket) = &inner.socket {
                    let line = serde_json::to_string(&frame.to_value()).expect("frame serializes");
                    let _ = socket.send_with_str(&line);
                }
            }
            Self::report(&inner, State::Connected);
        });
        let on_message = self.bind(|transport, event| {
            let Ok(message) = event.dyn_into::<MessageEvent>() else {
                return;
            };
            let Some(text) = message.data().as_string() else {
                return;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                return;
            };
            if let Some(frame) = Frame::parse(&value) {
                let inner = transport.inner.borrow();
                if let Some(events) = &inner.events {
                    let _ = events.try_send(Event::Frame(frame));
                }
            }
        });
        let on_close = self.bind(|transport, _| {
            let delay = {
                let mut inner = transport.inner.borrow_mut();
                inner.socket = None;
                if inner.closed {
                    return;
                }
                let delay = inner.backoff.retry_delay();
                inner.attempt += 1;
                Self::report(
                    &inner,
                    State::Reconnecting {
                        attempt: inner.attempt,
                        delay,
                    },
                );
                delay
            };
            transport.schedule(delay, |transport| transport.dial());
        });
        let on_error = self.bind(|_, _| {
            // The status of a failed handshake is invisible here; the
            // close callback schedules the retry.
        });
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        let mut inner = self.inner.borrow_mut();
        if inner.closed {
            drop(inner);
            let _ = socket.close();
            return;
        }
        inner.socket = Some(socket);
        // The last link's socket is closed, so its callbacks never run
        // again.
        inner.keepalive = vec![on_open, on_message, on_close, on_error];
    }

    /// Builds one browser callback bound to this transport; the
    /// caller registers it with the socket and keeps it in
    /// `Inner::keepalive` so it outlives the dial.
    fn bind(&self, run: impl Fn(&Dialer, JsValue) + 'static) -> Closure<dyn FnMut(JsValue)> {
        let transport = self.clone();
        Closure::new(move |event: JsValue| run(&transport, event))
    }

    /// Schedules `run` on the browser timer after `delay`.
    fn schedule(&self, delay: Duration, run: impl FnOnce(&Dialer) + 'static) {
        let transport = self.clone();
        let fire = Closure::once(move || run(&transport));
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                fire.as_ref().unchecked_ref(),
                delay.as_millis().min(i32::MAX as u128) as i32,
            );
        }
        fire.forget();
    }

    /// Shuts the link down and reports [`State::Closed`]. Safe to call
    /// from inside a callback: a contended borrow re-arms the
    /// shutdown on the next tick instead of dialing or panicking.
    fn shutdown(&self) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            self.schedule(Duration::ZERO, |transport| transport.shutdown());
            return;
        };
        if inner.closed {
            return;
        }
        inner.closed = true;
        inner.backlog.clear();
        if let Some(socket) = inner.socket.take() {
            socket.set_onopen(None);
            socket.set_onmessage(None);
            socket.set_onclose(None);
            socket.set_onerror(None);
            let _ = socket.close();
        }
        inner.keepalive.clear();
        drop(inner);
        let inner = self.inner.borrow();
        Self::report(&inner, State::Closed);
    }
}

impl Transport for WebTransport {
    fn link(&self) -> Link {
        Link::serve(&self.dialer.url)
    }

    fn start(&mut self, events: EventSender) {
        self.dialer.inner.borrow_mut().events = Some(events);
        self.dialer.dial();
    }

    fn send(&self, frame: Frame) {
        let mut inner = self.dialer.inner.borrow_mut();
        let Some(socket) = &inner.socket else {
            if inner.backlog.len() >= BACKLOG_CAP {
                inner.backlog.remove(0);
                crate::warn(
                    "the engine is unreachable; the reconnect backlog overflowed and the oldest queued frame was dropped",
                );
            }
            inner.backlog.push(frame);
            return;
        };
        let line = serde_json::to_string(&frame.to_value()).expect("frame serializes");
        let _ = socket.send_with_str(&line);
    }

    fn close(&self) {
        self.dialer.shutdown();
    }
}

impl Drop for WebTransport {
    fn drop(&mut self) {
        self.dialer.shutdown();
    }
}

/// Resolves after `millis`, driven by a browser timer. The replay
/// transport paces its recording with this on wasm, where blocking
/// sleeps do not exist.
pub(crate) async fn sleep(millis: u32) {
    let (done, wait) = async_channel::bounded::<()>(1);
    let Some(window) = web_sys::window() else {
        return;
    };
    let fire = Closure::once(move || {
        let _ = done.try_send(());
    });
    if window
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            fire.as_ref().unchecked_ref(),
            millis as i32,
        )
        .is_err()
    {
        return;
    }
    fire.forget();
    let _ = wait.recv().await;
}
