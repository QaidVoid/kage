//! Host-owned external ACP sessions.

#[cfg(target_os = "linux")]
mod lifecycle;
#[cfg(target_os = "linux")]
mod recorder;

/// What an external session is started from.
pub(crate) struct Launch<'a> {
    pub(crate) id: String,
    pub(crate) name: &'a str,
    pub(crate) cwd: &'a str,
    pub(crate) config: &'a kage_core::config::AcpAgent,
    pub(crate) owner: u64,
    pub(crate) peer: kage_acp::agent::AgentPeer,
    pub(crate) record: Option<Recording>,
    pub(crate) restore: Option<Restore>,
}

/// An upstream session to restore instead of creating one. It is restored
/// only through a capability the fresh agent process advertises, never by
/// creating a new session or sending local history as context. Resume is
/// preferred. History an upstream load replays is discarded, because clients
/// are shown the local display history instead.
pub(crate) struct Restore {
    /// The upstream session id recorded in the session header.
    pub(crate) upstream: String,
    /// Whether the restoring attachment first receives the display history.
    pub(crate) history: bool,
}

/// Where to record presentation history.
pub(crate) enum Recording {
    /// A new file. The header's backend identity is filled in once the
    /// upstream session exists.
    Create {
        path: std::path::PathBuf,
        header: Box<kage_session::entry::Header>,
    },
    /// The existing file of a restored session, opened with its lock held
    /// before the agent starts, so no other process restores it meanwhile.
    Append {
        writer: Box<kage_session::SessionWriter>,
    },
}

/// External agent processes a host runs at once, including ones cleaning up.
pub(super) const MAX_SESSIONS: usize = 16;

#[cfg(target_os = "linux")]
mod unix {

    use std::collections::HashMap;
    use std::io::{self, BufReader, Read};
    use std::process::{Command, Stdio};

    use super::lifecycle::{Lifecycles, Slot};
    use super::recorder::{Recorder, Target, bounded, event_size, presentation};
    use super::{Launch, Recording, Restore};
    use kage_core::reaper::spawn_protected;
    use kage_session::entry::{ExternalAcpPresentation, PresentationText, PresentationTurnOutcome};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use kage_acp::acp::{
        ClientCapabilities, ContentBlock, InitializeRequest, NewSessionRequest, PROTOCOL_VERSION,
        PromptCapabilities, PromptDelivery, PromptRequest, PromptResponse,
    };
    use kage_acp::agent::AgentPeer;
    use kage_acp::connection::{Message, UnixConnection, UnixPromptCanceller};
    use kage_core::{CancelFlag, sync::lock};
    use kage_jsonrpc::{ResponseBarrier, RpcError};
    use serde_json::{Value, json};

    const TIMEOUT: Duration = Duration::from_secs(10);
    const SESSION_BUDGET: usize = 8 * 1024 * 1024;
    const ATTACHMENT_BUDGET: usize = 2 * 1024 * 1024;
    const MAX_ATTACHMENTS: usize = 32;
    // Display history kept in memory for loads, below the attachment budget
    // so live updates still fit behind it.
    const HISTORY_BYTES: usize = 1024 * 1024;
    // Outstanding upstream permission asks per session, one waiter each.
    const MAX_ASKS: usize = 4;
    // Unresolved opens of one attachment, so concurrent opens stay bounded.
    const MAX_OPEN_RECEIPTS: u8 = 8;
    // Detached writers may still be finishing a write, so allow headroom.
    const MAX_WRITERS: usize = 2 * MAX_ATTACHMENTS;
    // Includes the active prompt and all registered tickets, until each is dropped.
    // Stay below the 32 ACP workers per connection so one session cannot occupy all
    // of them. Multiple external sessions can still exhaust the shared workers.
    const MAX_PROMPTS: usize = 16;

    // Account for value nodes and string storage, not just wire bytes.
    fn value_size(value: &Value) -> usize {
        let contents = match value {
            Value::String(text) => text.capacity(),
            Value::Array(items) => {
                items.capacity() * std::mem::size_of::<Value>()
                    + items
                        .iter()
                        .map(|item| value_size(item) - std::mem::size_of::<Value>())
                        .sum::<usize>()
            }
            Value::Object(items) => items
                .iter()
                .map(|(key, value)| key.capacity() + value_size(value) + 64)
                .sum(),
            _ => 0,
        };
        std::mem::size_of::<Value>().saturating_add(contents)
    }

    fn reserve(counter: &AtomicUsize, size: usize, limit: usize) -> bool {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(size).filter(|total| *total <= limit)
            })
            .is_ok()
    }

    struct Buffered {
        value: Value,
        size: usize,
        session: Arc<AtomicUsize>,
        attachment: Option<Arc<AtomicUsize>>,
    }

    impl Buffered {
        fn new(
            value: Value,
            session: &Arc<AtomicUsize>,
            attachment: Option<&Arc<AtomicUsize>>,
        ) -> Option<Self> {
            let size = value_size(&value);
            if !reserve(session, size, SESSION_BUDGET) {
                return None;
            }
            if let Some(counter) = attachment
                && !reserve(counter, size, ATTACHMENT_BUDGET)
            {
                session.fetch_sub(size, Ordering::AcqRel);
                return None;
            }
            Some(Self {
                value,
                size,
                session: Arc::clone(session),
                attachment: attachment.cloned(),
            })
        }
    }

    impl Drop for Buffered {
        fn drop(&mut self) {
            self.session.fetch_sub(self.size, Ordering::AcqRel);
            if let Some(counter) = &self.attachment {
                counter.fetch_sub(self.size, Ordering::AcqRel);
            }
        }
    }

    struct InterruptibleReader<R> {
        reader: R,
        stopped: Arc<AtomicBool>,
    }

    impl<R: Read> Read for InterruptibleReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                if self.stopped.load(Ordering::Acquire) {
                    return Ok(0);
                }
                match self.reader.read(buffer) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    result => return result,
                }
            }
        }
    }

    struct Attachment {
        sender: Option<mpsc::SyncSender<WriteItem>>,
        #[cfg(test)]
        worker_done: Option<mpsc::Receiver<()>>,
        identity: Arc<()>,
        epoch: u64,
        announced: bool,
        // Whether the downstream peer can take forwarded permission asks.
        asks: bool,
        // Whether the display history was queued for this attachment.
        shown_history: bool,
        receipts: u8,
        pending: Vec<Buffered>,
        bytes: Arc<AtomicUsize>,
        detached: Arc<AtomicBool>,
    }

    enum WriteItem {
        Update(Buffered),
        Batch(Vec<Buffered>),
        Barrier(mpsc::Sender<()>),
        Ask {
            params: Buffered,
            ask: Arc<Ask>,
            dispatched: mpsc::SyncSender<kage_acp::agent_peer::DispatchedRequest>,
        },
    }

    /// One upstream permission request, bound to the prompt that was active
    /// when it arrived and forwarded only to that prompt's owner.
    struct Ask {
        prompt: Arc<()>,
        owner: u64,
        options: Vec<String>,
        cancel: CancelFlag,
        reply: Reply,
    }

    // The upstream request handle, answered exactly once by whoever takes it.
    type Reply = Arc<Mutex<Option<kage_acp::connection::Request>>>;

    #[allow(
        clippy::struct_excessive_bools,
        reason = "independent flags guarded by the session lock"
    )]
    struct State {
        upstream: Option<String>,
        early: Vec<Buffered>,
        bytes: Arc<AtomicUsize>,
        attachments: HashMap<u64, Attachment>,
        active: Option<Active>,
        closed: bool,
        cancels: Option<mpsc::Sender<CancelAction>>,
        writers: Arc<AtomicUsize>,
        asks: Vec<Arc<Ask>>,
        recorder: Option<Recorder>,
        // The newest display history, sent to an attachment opened by a load.
        history: std::collections::VecDeque<(ExternalAcpPresentation, usize)>,
        history_bytes: usize,
        // Whether older history was dropped from memory.
        history_cut: bool,
        // Unique to this process, so item ids differ from a restored record's.
        run: String,
        turn: u64,
        // A recorded prompt whose end is not recorded yet.
        turn_open: bool,
        // An upstream load is replaying history that clients never see.
        discard_replay: bool,
    }

    impl State {
        // Withdraw asks; each waiter then answers its upstream request.
        fn cancel_asks(&self, matches: impl Fn(&Ask) -> bool) {
            for ask in self.asks.iter().filter(|ask| matches(ask)) {
                ask.cancel.cancel();
            }
        }

        fn record(&mut self, event: ExternalAcpPresentation) {
            self.remember(event.clone());
            if let Some(recorder) = &self.recorder {
                recorder.record(event);
            }
        }

        // Keep the newest display history within its budget.
        fn remember(&mut self, event: ExternalAcpPresentation) {
            let size = event_size(&event);
            self.history_bytes += size;
            self.history.push_back((event, size));
            while self.history_bytes > HISTORY_BYTES {
                let Some((_, size)) = self.history.pop_front() else {
                    break;
                };
                self.history_bytes -= size;
                self.history_cut = true;
            }
        }

        fn end_turn(&mut self, outcome: PresentationTurnOutcome) {
            if std::mem::take(&mut self.turn_open) {
                self.record(ExternalAcpPresentation::TurnEnd { outcome });
            }
        }

        // A turn still active when the session closes ended in failure.
        fn close(&mut self) {
            self.end_turn(PresentationTurnOutcome::Failed);
            self.recorder = None;
            self.closed = true;
            self.cancels = None;
            self.cancel_asks(|_| true);
            if let Some(active) = &self.active {
                active.abort.cancel();
            }
            self.early.clear();
            for attachment in self.attachments.values_mut() {
                attachment.detached.store(true, Ordering::Release);
                attachment.sender.take();
                attachment.pending.clear();
            }
        }
    }

    /// Registration for one attachment at a particular cancellation generation.
    pub(crate) struct PromptTicket {
        owner: u64,
        identity: Arc<()>,
        epoch: u64,
        admitted: Arc<AtomicUsize>,
        unannounced: Option<Arc<AtomicUsize>>,
    }

    impl PromptTicket {
        fn validate_session(&self, admitted: &Arc<AtomicUsize>) -> Result<(), RpcError> {
            if Arc::ptr_eq(&self.admitted, admitted) {
                Ok(())
            } else {
                Err(RpcError::internal(
                    "prompt ticket belongs to another external session",
                ))
            }
        }
    }

    impl PromptTicket {
        fn valid(&self, state: &State) -> bool {
            self.attached(state) && state.attachments[&self.owner].epoch == self.epoch
        }

        fn attached(&self, state: &State) -> bool {
            !state.closed
                && state
                    .attachments
                    .get(&self.owner)
                    .is_some_and(|attachment| Arc::ptr_eq(&attachment.identity, &self.identity))
        }
    }

    impl Drop for PromptTicket {
        fn drop(&mut self) {
            self.admitted.fetch_sub(1, Ordering::AcqRel);
            if let Some(counter) = &self.unannounced {
                counter.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    struct Active {
        owner: u64,
        identity: Arc<()>,
        token: Arc<()>,
        cancelled: bool,
        canceller: Option<UnixPromptCanceller>,
        abort: CancelFlag,
        cancellation: Cancellation,
    }

    enum Cancellation {
        None,
        Pending(Arc<()>),
        Claimed {
            identity: Arc<()>,
            done: mpsc::Sender<()>,
            write_done: mpsc::Receiver<Result<(), RpcError>>,
        },
        Finished {
            done: mpsc::Sender<()>,
            result: Result<(), RpcError>,
        },
    }

    pub(crate) struct CancelAction {
        state: Arc<Mutex<State>>,
        identity: Arc<()>,
        canceller: UnixPromptCanceller,
        abort: CancelFlag,
    }

    impl Active {
        fn cancel(&mut self, state: &Arc<Mutex<State>>) -> Option<CancelAction> {
            if self.cancelled {
                return None;
            }
            self.cancelled = true;
            self.cancel_action(state)
        }

        fn cancel_action(&mut self, state: &Arc<Mutex<State>>) -> Option<CancelAction> {
            let canceller = self.canceller.clone()?;
            let identity = Arc::new(());
            self.cancellation = Cancellation::Pending(Arc::clone(&identity));
            Some(CancelAction {
                state: Arc::clone(state),
                identity,
                canceller,
                abort: self.abort.clone(),
            })
        }
    }

    type ClaimedCancel = (mpsc::Receiver<()>, mpsc::Sender<Result<(), RpcError>>);

    fn claim_cancel(action: &CancelAction) -> Option<ClaimedCancel> {
        let mut state = lock(&action.state);
        if state.closed {
            return None;
        }
        let active = state.active.as_mut()?;
        if !matches!(&active.cancellation, Cancellation::Pending(identity) if Arc::ptr_eq(identity, &action.identity))
        {
            return None;
        }
        let (done, receiver) = mpsc::channel();
        let (write_done, write_receiver) = mpsc::channel();
        active.cancellation = Cancellation::Claimed {
            identity: Arc::clone(&action.identity),
            done,
            write_done: write_receiver,
        };
        Some((receiver, write_done))
    }

    fn cancel_worker(slot: &Arc<Slot>) -> Result<mpsc::Sender<CancelAction>, RpcError> {
        let (sender, actions) = mpsc::channel::<CancelAction>();
        let owner_slot = Arc::clone(slot);
        slot.spawn("kage-external-cancel", move || {
            while let Ok(action) = actions.recv() {
                answer_withdrawn_asks(&action.state, &owner_slot);
                run_cancel(&action);
            }
        })
        .map_err(|e| RpcError::internal(format!("cannot start external cancellation: {e}")))?;
        Ok(sender)
    }

    fn run_cancel(action: &CancelAction) {
        let Some((done, write_done)) = claim_cancel(action) else {
            return;
        };
        let deadline = Instant::now() + TIMEOUT;
        let cancel_result = action
            .canceller
            .cancel_if_active(deadline, None)
            .map(|_| ());
        let failed = cancel_result.is_err();
        {
            let mut state = lock(&action.state);
            if let Some(active) = &mut state.active
                && matches!(&active.cancellation, Cancellation::Claimed { identity, .. } if Arc::ptr_eq(identity, &action.identity))
                && let Cancellation::Claimed { done, .. } =
                    std::mem::replace(&mut active.cancellation, Cancellation::None)
            {
                active.cancellation = Cancellation::Finished {
                    done,
                    result: cancel_result.clone(),
                };
            }
        }
        let _ = write_done.send(cancel_result);
        if failed
            || matches!(
                done.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                Err(mpsc::RecvTimeoutError::Timeout)
            )
        {
            action.abort.cancel();
        }
    }

    struct Initialized {
        capabilities: PromptCapabilities,
        protocol_version: u32,
        upstream: String,
    }

    /// One unresolved open of an exact attachment, resolved exactly once.
    pub(crate) struct OpenTicket {
        session: Arc<External>,
        owner: u64,
        identity: Arc<()>,
        resolved: bool,
        // Acknowledges that a load's history reached the client.
        written: Option<mpsc::Receiver<()>>,
    }

    impl OpenTicket {
        pub(crate) fn session(&self) -> &Arc<External> {
            &self.session
        }

        /// Wait until a load's history is written, so the response follows
        /// it. Call with no host lock held.
        pub(crate) fn wait_history(&mut self) -> Result<(), RpcError> {
            match self.written.take() {
                Some(written) => written
                    .recv_timeout(TIMEOUT)
                    .map_err(|_| RpcError::internal("the session history was not delivered")),
                None => Ok(()),
            }
        }

        /// The open response was written, so updates may follow it.
        pub(crate) fn announce(mut self) {
            self.resolved = true;
            self.session.resolve_open(self.owner, &self.identity, true);
        }

        /// The open failed. Returns `Some(last)` if this detached the
        /// attachment, where `last` means the session closed.
        pub(crate) fn fail(mut self) -> Option<bool> {
            self.resolved = true;
            self.session.resolve_open(self.owner, &self.identity, false)
        }
    }

    impl Drop for OpenTicket {
        fn drop(&mut self) {
            if !self.resolved {
                self.session.resolve_open(self.owner, &self.identity, false);
            }
        }
    }

    /// A single upstream process and session, shared by host attachments.
    pub(crate) struct External {
        id: String,
        connection: UnixConnection,
        prompt_gate: Mutex<()>,
        admitted: Arc<AtomicUsize>,
        unannounced: Arc<AtomicUsize>,
        slot: Arc<Slot>,
        capabilities: PromptCapabilities,
        state: Arc<Mutex<State>>,
    }

    impl External {
        /// Start the agent in a reserved slot, which owns its cleanup. The
        /// creating owner is attached with an open that the caller resolves
        /// like any other once the creation response is written.
        pub(crate) fn start(
            lifecycles: &Lifecycles,
            launch: Launch<'_>,
        ) -> Result<(Arc<Self>, OpenTicket), RpcError> {
            let Launch {
                id,
                name,
                cwd,
                config,
                owner,
                peer,
                record,
                restore,
            } = launch;
            let startup = lifecycles.reserve()?;
            let (mut connection, incoming) = spawn_agent(&startup, name, cwd, config)?;
            let slot = startup.slot();
            let writers = Arc::new(AtomicUsize::new(0));
            let mut first = attachment(peer, slot, &writers)?;
            first.receipts = 1;
            let identity = Arc::clone(&first.identity);
            let cancels = cancel_worker(slot)?;
            let state = Arc::new(Mutex::new(State {
                upstream: None,
                early: Vec::new(),
                bytes: Arc::new(AtomicUsize::new(0)),
                attachments: HashMap::from([(owner, first)]),
                active: None,
                closed: false,
                cancels: Some(cancels),
                writers,
                asks: Vec::new(),
                recorder: None,
                history: std::collections::VecDeque::new(),
                history_bytes: 0,
                history_cut: false,
                run: kage_session::entry::EntryId::new().to_string(),
                turn: 0,
                turn_open: false,
                discard_replay: false,
            }));
            let session_id = id.clone();
            let started = receive_updates(incoming, Arc::clone(&state), id, slot)
                .and_then(|()| initialize(&mut connection, &state, cwd, restore.as_ref()))
                .and_then(|initialized| {
                    let seed = match &record {
                        Some(Recording::Append { writer }) => {
                            let replay =
                                kage_session::external_replay::replay_external(writer.path())
                                    .map_err(|e| {
                                        RpcError::internal(format!(
                                            "cannot read the external session record: {e}"
                                        ))
                                    })?;
                            let mut events = replay.events;
                            if replay.truncated {
                                events.push(ExternalAcpPresentation::Gap);
                            }
                            events
                        }
                        _ => Vec::new(),
                    };
                    let recorder = record
                        .map(|record| start_recording(slot, record, name, &initialized))
                        .transpose()?;
                    let history = restore
                        .as_ref()
                        .is_some_and(|restore| restore.history)
                        .then_some(owner);
                    let written =
                        publish(&state, &session_id, &initialized, recorder, seed, history)?;
                    Ok((initialized.capabilities, written))
                });
            let (capabilities, written) = match started {
                Ok(started) => started,
                Err(error) => {
                    let mut state = lock(&state);
                    if let Some(recorder) = state.recorder.take() {
                        recorder.discard();
                    }
                    state.close();
                    return Err(error);
                }
            };
            let session = Arc::new(Self {
                id: session_id,
                connection,
                prompt_gate: Mutex::new(()),
                admitted: Arc::new(AtomicUsize::new(0)),
                unannounced: Arc::new(AtomicUsize::new(0)),
                slot: startup.commit(),
                capabilities,
                state,
            });
            let created = OpenTicket {
                session: Arc::clone(&session),
                owner,
                identity,
                resolved: false,
                written,
            };
            Ok((session, created))
        }

        #[cfg(test)]
        pub(crate) fn attach(&self, owner: u64, peer: AgentPeer) -> Result<(), RpcError> {
            self.attach_locked(&mut lock(&self.state), owner, peer)
                .map(drop)
        }

        fn attach_locked<'a>(
            &self,
            state: &'a mut State,
            owner: u64,
            peer: AgentPeer,
        ) -> Result<&'a Attachment, RpcError> {
            if state.closed {
                return Err(RpcError::internal("external session is closed"));
            }
            if !state.attachments.contains_key(&owner) {
                if state.attachments.len() >= MAX_ATTACHMENTS {
                    return Err(RpcError::internal("too many external attachments"));
                }
                let attachment = attachment(peer, &self.slot, &state.writers)?;
                state.attachments.insert(owner, attachment);
            }
            Ok(&state.attachments[&owner])
        }

        /// Attach `owner` if needed and start one open of that exact attachment.
        /// The ticket must be announced after the open response is written.
        /// Dropping it instead fails the open.
        pub(crate) fn attach_open(
            self: &Arc<Self>,
            owner: u64,
            peer: AgentPeer,
        ) -> Result<OpenTicket, RpcError> {
            self.open_attachment(owner, peer, false)
        }

        /// [`Self::attach_open`] for a load. A new attachment first receives
        /// the display history, ahead of any live update.
        pub(crate) fn attach_load(
            self: &Arc<Self>,
            owner: u64,
            peer: AgentPeer,
        ) -> Result<OpenTicket, RpcError> {
            self.open_attachment(owner, peer, true)
        }

        fn open_attachment(
            self: &Arc<Self>,
            owner: u64,
            peer: AgentPeer,
            history: bool,
        ) -> Result<OpenTicket, RpcError> {
            let mut state = lock(&self.state);
            // A load must be the only open of its attachment, or another
            // open's announcement could release live updates before this
            // load's history and response.
            if history
                && state
                    .attachments
                    .get(&owner)
                    .is_some_and(|attachment| attachment.announced || attachment.receipts > 0)
            {
                return Err(RpcError::new(
                    -32602,
                    "the session is already open on this connection; close it before loading it again",
                ));
            }
            self.attach_locked(&mut state, owner, peer)?;
            let mut written = None;
            if history && !state.attachments[&owner].shown_history {
                written = Some(
                    queue_history(&mut state, owner, &self.id)
                        .ok_or_else(|| RpcError::internal("cannot send the session history"))?,
                );
            }
            let attachment = state.attachments.get_mut(&owner).expect("attached owner");
            if attachment.receipts >= MAX_OPEN_RECEIPTS {
                return Err(RpcError::internal("too many pending external opens"));
            }
            attachment.receipts += 1;
            Ok(OpenTicket {
                session: Arc::clone(self),
                owner,
                identity: Arc::clone(&attachment.identity),
                resolved: false,
                written,
            })
        }

        #[cfg(test)]
        pub(crate) fn owners(&self) -> Vec<u64> {
            lock(&self.state).attachments.keys().copied().collect()
        }

        #[cfg(test)]
        pub(crate) fn announce(&self, owner: u64) {
            let mut state = lock(&self.state);
            self.announce_locked(&mut state, owner);
        }

        // Resolve one open of the exact attachment. A failed open detaches the
        // attachment only while it is unannounced and has no other open. Returns
        // whether that detach happened and, if so, whether it was the last.
        fn resolve_open(&self, owner: u64, identity: &Arc<()>, announce: bool) -> Option<bool> {
            let mut state = lock(&self.state);
            let attachment = state
                .attachments
                .get_mut(&owner)
                .filter(|attachment| Arc::ptr_eq(&attachment.identity, identity))?;
            attachment.receipts = attachment.receipts.saturating_sub(1);
            if announce {
                self.announce_locked(&mut state, owner);
                return None;
            }
            if attachment.announced || attachment.receipts > 0 {
                return None;
            }
            self.detach_locked(&mut state, owner)
        }

        fn announce_locked(&self, state: &mut State, owner: u64) {
            if state.closed {
                return;
            }
            let mut failed = false;
            if let Some(attachment) = state.attachments.get_mut(&owner)
                && !attachment.announced
            {
                attachment.announced = true;
                let pending = std::mem::take(&mut attachment.pending);
                // One item, so a long history cannot fill the writer channel.
                failed = !pending.is_empty()
                    && attachment
                        .sender
                        .as_ref()
                        .expect("open attachment")
                        .try_send(WriteItem::Batch(pending))
                        .is_err();
            }
            if failed {
                self.close(state);
            }
        }

        /// Detach `owner` without blocking. The last detach marks the session
        /// terminal, and the slot finishes cleanup in the background. Otherwise
        /// the owner's active prompt is handed to the cancellation worker.
        pub(crate) fn detach(&self, owner: u64) -> bool {
            self.detach_locked(&mut lock(&self.state), owner)
                .unwrap_or(false)
        }

        fn detach_locked(&self, state: &mut State, owner: u64) -> Option<bool> {
            let attachment = state.attachments.remove(&owner)?;
            state.cancel_asks(|ask| ask.owner == owner);
            attachment.detached.store(true, Ordering::Release);
            let last = state.attachments.is_empty();
            if last {
                self.close(state);
            } else if let Some(action) = state
                .active
                .as_mut()
                .filter(|active| {
                    active.owner == owner && Arc::ptr_eq(&active.identity, &attachment.identity)
                })
                .and_then(|active| active.cancel(&self.state))
            {
                self.queue_cancel(state, action);
            }
            Some(last)
        }

        // Closing only marks state and the slot. It never waits for cleanup.
        fn close(&self, state: &mut State) {
            state.close();
            self.slot.terminate();
        }

        fn terminate(&self) {
            self.close(&mut lock(&self.state));
        }

        // A cancellation that cannot reach its worker cannot be resolved, so
        // the session fails closed.
        fn queue_cancel(&self, state: &mut State, action: CancelAction) {
            let queued = state
                .cancels
                .as_ref()
                .is_some_and(|cancels| cancels.send(action).is_ok());
            if !queued && !state.closed {
                self.close(state);
            }
        }

        pub(crate) fn register_prompt(&self, owner: u64) -> Result<PromptTicket, RpcError> {
            let state = lock(&self.state);
            let Some(attachment) = state.attachments.get(&owner).filter(|_| !state.closed) else {
                return Err(RpcError::new(-32602, "external session is not attached"));
            };
            // Keep one session slot available to already announced attachments.
            // Reservations made before announcement last for the ticket lifetime.
            let unannounced = if attachment.announced {
                None
            } else if reserve(&self.unannounced, 1, MAX_PROMPTS - 1) {
                Some(Arc::clone(&self.unannounced))
            } else {
                return Err(RpcError::internal("too many unannounced external prompts"));
            };
            if !reserve(&self.admitted, 1, MAX_PROMPTS) {
                if let Some(counter) = &unannounced {
                    counter.fetch_sub(1, Ordering::AcqRel);
                }
                return Err(RpcError::internal("too many external prompts"));
            }
            Ok(PromptTicket {
                owner,
                identity: Arc::clone(&attachment.identity),
                epoch: attachment.epoch,
                admitted: Arc::clone(&self.admitted),
                unannounced,
            })
        }

        #[allow(
            clippy::needless_pass_by_value,
            reason = "a prompt ticket is single-use"
        )]
        pub(crate) fn prompt(
            &self,
            ticket: PromptTicket,
            mut req: PromptRequest,
        ) -> Result<PromptResponse, RpcError> {
            ticket.validate_session(&self.admitted)?;
            let owner = ticket.owner;
            if req.delivery == Some(PromptDelivery::Steer) {
                return Err(RpcError::new(
                    -32602,
                    "external session steering is not supported; queue a prompt instead",
                ));
            }
            self.check_content(&req)?;
            if !self.wait_for_announcement(&ticket, TIMEOUT)? {
                return Ok(PromptResponse {
                    stop_reason: kage_acp::acp::StopReason::Cancelled,
                });
            }
            let abort = CancelFlag::new();
            let Some(_gate) = self.wait_prompt_gate(&ticket) else {
                return Ok(PromptResponse {
                    stop_reason: kage_acp::acp::StopReason::Cancelled,
                });
            };
            {
                let mut state = lock(&self.state);
                if !ticket.valid(&state) {
                    return Ok(PromptResponse {
                        stop_reason: kage_acp::acp::StopReason::Cancelled,
                    });
                }
                req.session_id = state.upstream.clone().expect("initialized upstream");
                begin_turn(&mut state, &req);
                state.active = Some(Active {
                    owner,
                    identity: Arc::clone(&ticket.identity),
                    token: Arc::new(()),
                    cancelled: false,
                    canceller: None,
                    abort: abort.clone(),
                    cancellation: Cancellation::None,
                });
            }
            req.delivery = None;
            let deadline = Instant::now() + TIMEOUT;
            // Cancellation can race with dispatch before the upstream canceller exists.
            // The pending cancellation is sent once dispatch supplies that canceller.
            let prompt = match self.connection.dispatch_prompt(&req, Some(deadline), None) {
                Ok(prompt) => prompt,
                Err(error) => {
                    let mut state = lock(&self.state);
                    self.close(&mut state);
                    state.active = None;
                    return Err(error);
                }
            };
            {
                let mut state = lock(&self.state);
                let active = state.active.as_mut().expect("active prompt");
                active.canceller = Some(prompt.canceller());
                if active.cancelled
                    && let Some(action) = active.cancel_action(&self.state)
                {
                    self.queue_cancel(&mut state, action);
                }
            }
            let mut result = prompt.wait(None, TIMEOUT, Some(&abort));
            let active = {
                let mut state = lock(&self.state);
                let active = state.active.take().expect("active prompt");
                state.cancel_asks(|ask| Arc::ptr_eq(&ask.prompt, &active.token));
                active
            };
            let cancelled = active.cancelled;
            if self.cancellation_write_failed(active.cancellation) {
                self.end_turn(PresentationTurnOutcome::Failed);
                self.terminate();
                return Err(RpcError::internal(
                    "external cancellation write did not finish",
                ));
            }
            if let Ok(response) = &mut result
                && cancelled
            {
                response.stop_reason = kage_acp::acp::StopReason::Cancelled;
            }
            // The prompt gate is still held, so the next turn cannot start
            // before this one is recorded as ended.
            let outcome = match &result {
                Ok(response) => turn_outcome(response.stop_reason),
                Err(_) => PresentationTurnOutcome::Failed,
            };
            self.end_turn(outcome);
            if result.is_err() {
                self.terminate();
                return result;
            }
            self.wait_for_writes(&ticket)?;
            result
        }

        fn check_content(&self, req: &PromptRequest) -> Result<(), RpcError> {
            for content in &req.prompt {
                let supported = match content {
                    ContentBlock::Text(_) | ContentBlock::ResourceLink(_) => true,
                    ContentBlock::Image(_) => self.capabilities.image,
                    ContentBlock::Audio(_) => self.capabilities.audio,
                    ContentBlock::Resource(_) => self.capabilities.embedded_context,
                    ContentBlock::Unknown => false,
                };
                if !supported {
                    return Err(RpcError::new(
                        -32602,
                        "external agent does not support this prompt content",
                    ));
                }
            }
            Ok(())
        }

        fn end_turn(&self, outcome: PresentationTurnOutcome) {
            lock(&self.state).end_turn(outcome);
        }

        /// Whether the presentation history has no gap so far.
        pub(crate) fn recording_complete(&self) -> bool {
            lock(&self.state)
                .recorder
                .as_ref()
                .is_some_and(Recorder::complete)
        }

        fn cancellation_write_failed(&self, cancellation: Cancellation) -> bool {
            match cancellation {
                Cancellation::Finished { done, result } => {
                    drop(done);
                    result.is_err()
                }
                Cancellation::Claimed {
                    done, write_done, ..
                } => {
                    drop(done);
                    loop {
                        match write_done.recv_timeout(Duration::from_millis(25)) {
                            Ok(result) => break result.is_err(),
                            Err(mpsc::RecvTimeoutError::Disconnected) => break true,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if lock(&self.state).closed {
                                    break true;
                                }
                            }
                        }
                    }
                }
                Cancellation::None | Cancellation::Pending(_) => false,
            }
        }

        // A valid, already announced attachment needs no wait. Otherwise the
        // deadline starts after the first check. Cancellation or detachment wins
        // over timeout; timeout wins over a subsequently observed announcement.
        fn wait_for_announcement(
            &self,
            ticket: &PromptTicket,
            timeout: Duration,
        ) -> Result<bool, RpcError> {
            {
                let state = lock(&self.state);
                if !ticket.valid(&state) {
                    return Ok(false);
                }
                if state.attachments[&ticket.owner].announced {
                    return Ok(true);
                }
            }
            self.wait_for_announcement_until(ticket, Instant::now() + timeout)
        }

        fn wait_for_announcement_until(
            &self,
            ticket: &PromptTicket,
            deadline: Instant,
        ) -> Result<bool, RpcError> {
            loop {
                {
                    let state = lock(&self.state);
                    if !ticket.valid(&state) {
                        return Ok(false);
                    }
                    // At the deadline, timeout wins even if announcement is visible.
                    if Instant::now() >= deadline {
                        return Err(RpcError::internal(
                            "external session announcement timed out",
                        ));
                    }
                    if state.attachments[&ticket.owner].announced {
                        return Ok(true);
                    }
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                // Poll only on the prompt worker. The inbound path never waits for a receipt.
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
        }

        fn wait_prompt_gate(&self, ticket: &PromptTicket) -> Option<std::sync::MutexGuard<'_, ()>> {
            loop {
                match self.prompt_gate.try_lock() {
                    Ok(gate) => return Some(gate),
                    Err(std::sync::TryLockError::Poisoned(error)) => {
                        return Some(error.into_inner());
                    }
                    Err(std::sync::TryLockError::WouldBlock) => {
                        let state = lock(&self.state);
                        if !ticket.valid(&state) {
                            return None;
                        }
                        drop(state);
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }

        fn wait_for_writes(&self, ticket: &PromptTicket) -> Result<(), RpcError> {
            let (tx, rx) = mpsc::channel();
            {
                let mut state = lock(&self.state);
                if !ticket.attached(&state) {
                    return Err(RpcError::internal(
                        "external session was closed or detached",
                    ));
                }
                let attachment = state
                    .attachments
                    .get(&ticket.owner)
                    .expect("validated attachment");
                if !attachment.announced {
                    return Err(RpcError::internal("external session is not announced"));
                }
                if attachment
                    .sender
                    .as_ref()
                    .expect("open attachment")
                    .try_send(WriteItem::Barrier(tx))
                    .is_err()
                {
                    self.close(&mut state);
                    return Err(RpcError::internal(
                        "external notification queue unavailable",
                    ));
                }
            }
            let deadline = Instant::now() + TIMEOUT;
            loop {
                if Instant::now() >= deadline {
                    self.close_if_ticket_current(ticket);
                    return Err(RpcError::internal("external notification write timed out"));
                }
                match rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(()) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.close_if_ticket_current(ticket);
                        return Err(RpcError::internal(
                            "external notification writer disconnected",
                        ));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if !ticket.attached(&lock(&self.state)) {
                            return Err(RpcError::internal(
                                "external session was closed or detached",
                            ));
                        }
                    }
                }
            }
            if !ticket.attached(&lock(&self.state)) {
                return Err(RpcError::internal(
                    "external session was closed or detached",
                ));
            }
            Ok(())
        }

        fn close_if_ticket_current(&self, ticket: &PromptTicket) {
            let mut state = lock(&self.state);
            if ticket.attached(&state) {
                self.close(&mut state);
            }
        }

        /// Cancel `owner`'s prompts without blocking the inbound path.
        pub(crate) fn cancel(&self, owner: u64) {
            let mut state = lock(&self.state);
            let Some(attachment) = state.attachments.get_mut(&owner) else {
                return;
            };
            attachment.epoch = attachment.epoch.wrapping_add(1);
            let identity = Arc::clone(&attachment.identity);
            state.cancel_asks(|ask| ask.owner == owner);
            if let Some(action) = state
                .active
                .as_mut()
                .filter(|active| active.owner == owner && Arc::ptr_eq(&active.identity, &identity))
                .and_then(|active| active.cancel(&self.state))
            {
                self.queue_cancel(&mut state, action);
            }
        }
    }

    // Spawn the agent and connect to it, handing each resource to the slot as
    // soon as it exists.
    fn spawn_agent(
        startup: &super::lifecycle::Startup,
        name: &str,
        cwd: &str,
        config: &kage_core::config::AcpAgent,
    ) -> Result<(UnixConnection, kage_acp::connection::Incoming), RpcError> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = spawn_protected(&mut command).map_err(|e| {
            RpcError::internal(format!("cannot start external agent {name:?}: {e}"))
        })?;
        let stdout = child.take_stdout();
        let stdin = child.take_stdin();
        startup.child(child);
        let (Some(stdout), Some(stdin)) = (stdout, stdin) else {
            return Err(RpcError::internal("external agent pipes are unavailable"));
        };
        nix::fcntl::fcntl(
            &stdout,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .map_err(|e| RpcError::internal(format!("cannot configure external reader: {e}")))?;
        let reader_stopped = Arc::new(AtomicBool::new(false));
        let reader = InterruptibleReader {
            reader: stdout,
            stopped: Arc::clone(&reader_stopped),
        };
        let (connection, incoming, reader_thread) =
            UnixConnection::new_managed(BufReader::new(reader), stdin.into())
                .map_err(|e| RpcError::internal(format!("cannot connect external agent: {e}")))?;
        startup.transport(connection.shutdown_handle(), reader_stopped, reader_thread);
        Ok((connection, incoming))
    }

    // The worker closes the session when upstream input ends. The slot then
    // shuts down the transport, reaps the process, and joins the reader.
    fn receive_updates(
        incoming: kage_acp::connection::Incoming,
        sink: Arc<Mutex<State>>,
        external_id: String,
        slot: &Arc<Slot>,
    ) -> Result<(), RpcError> {
        let terminal = Closer {
            state: Arc::clone(&sink),
            slot: Arc::clone(slot),
        };
        let asks_slot = Arc::clone(slot);
        slot.spawn("kage-external-receive", move || {
            let _terminal = terminal;
            while let Ok(message) = incoming.recv() {
                match message {
                    Message::Notification { method, params } if method == "session/update" => {
                        let mut state = lock(&sink);
                        if state.closed {
                            break;
                        }
                        if state.upstream.is_none() {
                            if state.discard_replay && replayed(&params) {
                                continue;
                            }
                            if let Some(update) = Buffered::new(params, &state.bytes, None) {
                                state.early.push(update);
                            } else {
                                state.close();
                            }
                        } else {
                            forward(&mut state, &external_id, params);
                        }
                        if state.closed {
                            break;
                        }
                    }
                    Message::Request(request) if request.method == "session/request_permission" => {
                        if !admit_ask(&sink, &asks_slot, &external_id, request) {
                            break;
                        }
                    }
                    Message::Request(mut request) => {
                        let response = Err(RpcError::method_not_found(&request.method));
                        if request
                            .respond_bounded(response, Some(Instant::now() + TIMEOUT), None)
                            .is_err()
                        {
                            break;
                        }
                    }
                    Message::Notification { .. } => {}
                }
            }
            drop(incoming);
        })
        .map_err(|e| RpcError::internal(format!("cannot start external receiver: {e}")))
    }

    // Send the display history to an unannounced attachment opened by a
    // load. Returns the barrier that the history was written, or `None` if
    // it could not be queued. Fragments of one message are merged. The newest history is kept within half the
    // attachment budget, and a notice marks where older history was cut.
    fn queue_history(state: &mut State, owner: u64, id: &str) -> Option<mpsc::Receiver<()>> {
        let mut updates: Vec<Value> = Vec::new();
        let mut last_item: Option<(&'static str, String)> = None;
        for (event, _) in &state.history {
            let item = match event {
                ExternalAcpPresentation::AgentMessage { item_id, .. } => {
                    Some(("agent_message_chunk", item_id.as_str().to_owned()))
                }
                ExternalAcpPresentation::AgentThought { item_id, .. } => {
                    Some(("agent_thought_chunk", item_id.as_str().to_owned()))
                }
                _ => None,
            };
            let Some(update) = history_update(event, id) else {
                last_item = None;
                continue;
            };
            if item.is_some()
                && item == last_item
                && let Some(previous) = updates.last_mut()
                && let (Some(text), Some(more)) = (
                    previous["update"]["content"]["text"]
                        .as_str()
                        .map(str::to_owned),
                    update["update"]["content"]["text"].as_str(),
                )
            {
                previous["update"]["content"]["text"] = Value::String(text + more);
                continue;
            }
            last_item = item;
            updates.push(update);
        }
        let mut cut = state.history_cut;
        let State {
            attachments, bytes, ..
        } = state;
        let attachment = attachments.get_mut(&owner).expect("attached owner");
        let budget = ATTACHMENT_BUDGET / 2;
        let mut used = 0;
        let mut kept = Vec::new();
        for update in updates.into_iter().rev() {
            let size = value_size(&update);
            let reserved = (used + size <= budget)
                .then(|| Buffered::new(update, bytes, Some(&attachment.bytes)))
                .flatten();
            let Some(update) = reserved else {
                cut = true;
                break;
            };
            used += size;
            kept.push(update);
        }
        if cut && let Some(gap) = Buffered::new(history_gap(id), bytes, Some(&attachment.bytes)) {
            kept.push(gap);
        }
        // A load's history precedes its response, so it is written now, and
        // the opener waits for the barrier before answering. Live updates
        // stay pending until the response is written.
        let sender = attachment.sender.as_ref()?;
        let history: Vec<_> = kept.into_iter().rev().collect();
        if !history.is_empty() && sender.try_send(WriteItem::Batch(history)).is_err() {
            return None;
        }
        let (done, written) = mpsc::channel();
        sender.try_send(WriteItem::Barrier(done)).ok()?;
        Some(written)
    }

    // A recorded display event as the `session/update` it was shown as.
    fn history_update(event: &ExternalAcpPresentation, id: &str) -> Option<Value> {
        let text = |kind: &str, text: &PresentationText| json!({"sessionUpdate": kind, "content": {"type": "text", "text": text.as_str()}});
        let update = match event {
            ExternalAcpPresentation::Prompt { text: prompt } => text("user_message_chunk", prompt),
            ExternalAcpPresentation::AgentMessage { text: body, .. } => {
                text("agent_message_chunk", body)
            }
            ExternalAcpPresentation::AgentThought { text: body, .. } => {
                text("agent_thought_chunk", body)
            }
            ExternalAcpPresentation::ToolCall {
                tool_call_id,
                title,
                status,
            } => json!({"sessionUpdate": "tool_call", "toolCallId": tool_call_id.as_str(),
                "title": title.as_str(), "status": status_name(*status)}),
            ExternalAcpPresentation::ToolCallUpdate {
                tool_call_id,
                title,
                status,
                ..
            } => {
                let mut update = json!({"sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id.as_str()});
                if let Some(title) = title {
                    update["title"] = json!(title.as_str());
                }
                if let Some(status) = status {
                    update["status"] = json!(status_name(*status));
                }
                update
            }
            ExternalAcpPresentation::Gap => return Some(history_gap(id)),
            ExternalAcpPresentation::TurnEnd { .. } => return None,
        };
        Some(json!({"sessionId": id, "update": update}))
    }

    // Marked as a kage notice, since it is not text the agent sent.
    fn history_gap(id: &str) -> Value {
        json!({"sessionId": id, "update": {"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "(Part of this session's history was not recorded.)"},
            "_meta": {"kage": {"historyGap": true}}}})
    }

    // Updates an upstream load replays as history, which clients never see.
    fn replayed(params: &Value) -> bool {
        matches!(
            params["update"]["sessionUpdate"].as_str(),
            Some(
                "user_message_chunk"
                    | "agent_message_chunk"
                    | "agent_thought_chunk"
                    | "tool_call"
                    | "tool_call_update"
                    | "plan"
            )
        )
    }

    fn status_name(status: kage_session::entry::PresentationToolStatus) -> &'static str {
        use kage_session::entry::PresentationToolStatus;
        match status {
            PresentationToolStatus::Pending => "pending",
            PresentationToolStatus::InProgress => "in_progress",
            PresentationToolStatus::Completed => "completed",
            PresentationToolStatus::Failed | PresentationToolStatus::Unknown => "failed",
        }
    }

    // Start a turn and record its prompt text, before dispatch, so updates
    // the upstream sends for it are recorded after it.
    // Exactly one prompt record starts each turn, with its text blocks cut
    // to the presentation limit.
    fn begin_turn(state: &mut State, req: &PromptRequest) {
        state.turn += 1;
        let text: Vec<&str> = req
            .prompt
            .iter()
            .filter_map(|content| match content {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        state.record(ExternalAcpPresentation::Prompt {
            text: bounded(&text.join("\n")),
        });
        state.turn_open = true;
    }

    fn turn_outcome(stop: kage_acp::acp::StopReason) -> PresentationTurnOutcome {
        use kage_acp::acp::StopReason;
        match stop {
            StopReason::EndTurn => PresentationTurnOutcome::EndTurn,
            StopReason::MaxTokens => PresentationTurnOutcome::MaxTokens,
            StopReason::MaxTurnRequests => PresentationTurnOutcome::MaxTurnRequests,
            StopReason::Refusal => PresentationTurnOutcome::Refusal,
            StopReason::Cancelled => PresentationTurnOutcome::Cancelled,
        }
    }

    fn cancelled() -> Value {
        json!({"outcome": {"outcome": "cancelled"}})
    }

    // Admit one upstream permission request. Returns false once the upstream
    // can no longer be answered, which shuts the session down.
    fn admit_ask(
        sink: &Arc<Mutex<State>>,
        slot: &Arc<Slot>,
        id: &str,
        request: kage_acp::connection::Request,
    ) -> bool {
        let reply: Reply = Arc::new(Mutex::new(Some(request)));
        let queued = queue_ask(&mut lock(sink), sink, slot, id, &reply);
        queued || answer(&reply, None, None, sink, slot)
    }

    // Queue the ask on the active prompt owner's update stream, after the
    // updates that preceded it, with a waiter that answers it once.
    fn queue_ask(
        state: &mut State,
        sink: &Arc<Mutex<State>>,
        slot: &Arc<Slot>,
        id: &str,
        reply: &Reply,
    ) -> bool {
        if state.closed || state.asks.len() >= MAX_ASKS {
            return false;
        }
        let Some(active) = state.active.as_ref().filter(|active| !active.cancelled) else {
            return false;
        };
        let owner = active.owner;
        let prompt = Arc::clone(&active.token);
        let Some(attachment) = state.attachments.get(&owner).filter(|attachment| {
            attachment.announced
                && attachment.asks
                && Arc::ptr_eq(&attachment.identity, &active.identity)
        }) else {
            return false;
        };
        let Some(sender) = attachment.sender.clone() else {
            return false;
        };
        let mut params = {
            let mut request = lock(reply);
            let Some(request) = request.as_mut() else {
                return false;
            };
            if request.params.get("sessionId").and_then(Value::as_str) != state.upstream.as_deref()
            {
                return false;
            }
            std::mem::take(&mut request.params)
        };
        let Some(options) = offered(&params) else {
            return false;
        };
        params["sessionId"] = Value::String(id.to_owned());
        let Some(params) = Buffered::new(params, &state.bytes, Some(&attachment.bytes)) else {
            return false;
        };
        let ask = Arc::new(Ask {
            prompt,
            owner,
            options,
            cancel: CancelFlag::new(),
            reply: Arc::clone(reply),
        });
        let (dispatched, handle) = mpsc::sync_channel(1);
        let waiter = {
            let (ask, sink, owner_slot) = (Arc::clone(&ask), Arc::clone(sink), Arc::clone(slot));
            slot.spawn("kage-external-ask", move || {
                wait_ask(&ask, &handle, &sink, &owner_slot);
            })
        };
        if waiter.is_err() {
            return false;
        }
        state.asks.push(Arc::clone(&ask));
        let _ = sender.try_send(WriteItem::Ask {
            params,
            ask,
            dispatched,
        });
        true
    }

    // Offered option IDs, which must be present and unambiguous.
    fn offered(params: &Value) -> Option<Vec<String>> {
        let mut ids = Vec::new();
        for option in params.get("options")?.as_array()? {
            let id = option.get("optionId")?.as_str()?;
            if ids.iter().any(|other| other == id) {
                return None;
            }
            ids.push(id.to_owned());
        }
        (!ids.is_empty()).then_some(ids)
    }

    // Only an offered selection passes through, with its extension metadata.
    fn selected(ask: &Ask, value: &Value) -> Option<Value> {
        let outcome = value.get("outcome")?;
        if outcome.get("outcome")?.as_str()? != "selected" {
            return None;
        }
        let option = outcome.get("optionId")?.as_str()?;
        if !ask.options.iter().any(|offered| offered == option) {
            return None;
        }
        let mut result = json!({"outcome": {"outcome": "selected", "optionId": option}});
        if let Some(meta) = outcome.get("_meta") {
            result["outcome"]["_meta"] = meta.clone();
        }
        if let Some(meta) = value.get("_meta") {
            result["_meta"] = meta.clone();
        }
        Some(result)
    }

    // Answer the upstream request once. The decision is made under the
    // session lock, where cancellation is marked, so a cancelled ask is never
    // answered with a selection. A failed answer shuts the session down,
    // because the upstream would otherwise wait forever.
    fn answer(
        reply: &Reply,
        outcome: Option<Value>,
        ask: Option<&Ask>,
        sink: &Mutex<State>,
        slot: &Slot,
    ) -> bool {
        let (request, outcome) = {
            let _state = lock(sink);
            let outcome = outcome
                .filter(|_| !ask.is_some_and(|ask| ask.cancel.is_cancelled()))
                .unwrap_or_else(cancelled);
            (lock(reply).take(), outcome)
        };
        let Some(mut request) = request else {
            return true;
        };
        if request
            .respond_bounded(Ok(outcome), Some(Instant::now() + TIMEOUT), None)
            .is_ok()
        {
            return true;
        }
        lock(sink).close();
        slot.terminate();
        false
    }

    // Answer withdrawn asks upstream before the cancellation notice, so a
    // slow downstream withdrawal cannot hold back the upstream turn.
    fn answer_withdrawn_asks(state: &Mutex<State>, slot: &Slot) {
        let replies: Vec<Reply> = lock(state)
            .asks
            .iter()
            .filter(|ask| ask.cancel.is_cancelled())
            .map(|ask| Arc::clone(&ask.reply))
            .collect();
        for reply in replies {
            answer(&reply, None, None, state, slot);
        }
    }

    // A withdrawn ask is answered as cancelled at once. The waiter still
    // settles a request the writer dispatched, which withdraws it downstream.
    fn wait_ask(
        ask: &Ask,
        handle: &mpsc::Receiver<kage_acp::agent_peer::DispatchedRequest>,
        sink: &Arc<Mutex<State>>,
        slot: &Slot,
    ) {
        let _settle = Settle { ask, sink, slot };
        let dispatched = loop {
            match handle.recv_timeout(Duration::from_millis(25)) {
                Ok(request) => break Some(request),
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if ask.cancel.is_cancelled() {
                        answer(&ask.reply, None, None, sink, slot);
                    }
                }
            }
        };
        let outcome = dispatched
            .and_then(|request| request.wait(&ask.cancel).ok())
            .and_then(|value| selected(ask, &value));
        answer(&ask.reply, outcome, Some(ask), sink, slot);
    }

    // Answers the ask if its waiter unwinds, and frees its place.
    struct Settle<'a> {
        ask: &'a Ask,
        sink: &'a Mutex<State>,
        slot: &'a Slot,
    }

    impl Drop for Settle<'_> {
        fn drop(&mut self) {
            answer(&self.ask.reply, None, None, self.sink, self.slot);
            lock(self.sink)
                .asks
                .retain(|other| !std::ptr::eq(other.as_ref(), self.ask));
        }
    }

    // Closes the session when the receive worker exits, including by panic.
    struct Closer {
        state: Arc<Mutex<State>>,
        slot: Arc<Slot>,
    }

    impl Drop for Closer {
        fn drop(&mut self) {
            lock(&self.state).close();
            self.slot.terminate();
        }
    }

    fn ordered_result<T>(
        result: Result<T, RpcError>,
        barrier: Option<ResponseBarrier>,
        deadline: Instant,
        operation: &str,
    ) -> Result<T, RpcError> {
        match barrier {
            Some(barrier) => barrier
                .wait_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| RpcError::internal(format!("{operation} ordering barrier failed")))?,
            None if result.is_ok() => {
                return Err(RpcError::internal(format!(
                    "{operation} ordering barrier unavailable"
                )));
            }
            None => {}
        }
        result
    }

    fn initialize(
        connection: &mut UnixConnection,
        state: &Mutex<State>,
        cwd: &str,
        restore: Option<&Restore>,
    ) -> Result<Initialized, RpcError> {
        let deadline = Instant::now() + TIMEOUT;
        let (init, barrier) = connection.initialize(
            &InitializeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_capabilities: ClientCapabilities::default(),
                client_info: None,
            },
            Some(deadline),
            None,
        );
        let init = ordered_result(init, barrier, deadline, "initialize")?;
        let protocol_version = u32::try_from(init.protocol_version)
            .map_err(|_| RpcError::internal("invalid external protocol version"))?;
        let capabilities = init.agent_capabilities;
        if let Some(restore) = restore {
            let can_load = capabilities.load_session;
            let can_resume = capabilities.session_capabilities.resume.is_some();
            let load = can_load && !can_resume;
            if !load && !can_resume {
                return Err(RpcError::new(
                    -32602,
                    "the external agent cannot restore sessions",
                ));
            }
            let deadline = Instant::now() + TIMEOUT;
            lock(state).discard_replay = load;
            let restored = if load {
                let (response, barrier) = connection.load_session(
                    &kage_acp::acp::LoadSessionRequest {
                        session_id: restore.upstream.clone(),
                        cwd: cwd.to_owned(),
                        mcp_servers: vec![],
                        meta: None,
                    },
                    Some(deadline),
                    None,
                );
                ordered_result(response, barrier, deadline, "session/load").map(drop)
            } else {
                let (response, barrier) = connection.resume_session(
                    &kage_acp::acp::ResumeSessionRequest {
                        session_id: restore.upstream.clone(),
                        cwd: cwd.to_owned(),
                        mcp_servers: vec![],
                        meta: None,
                    },
                    Some(deadline),
                    None,
                );
                ordered_result(response, barrier, deadline, "session/resume").map(drop)
            };
            restored?;
            return Ok(Initialized {
                capabilities: capabilities.prompt_capabilities,
                protocol_version,
                upstream: restore.upstream.clone(),
            });
        }
        let deadline = Instant::now() + TIMEOUT;
        let (response, barrier) = connection.new_session(
            &NewSessionRequest {
                cwd: cwd.to_owned(),
                mcp_servers: vec![],
                meta: None,
            },
            Some(deadline),
            None,
        );
        let response = ordered_result(response, barrier, deadline, "session")?;
        Ok(Initialized {
            capabilities: capabilities.prompt_capabilities,
            protocol_version,
            upstream: response.session_id,
        })
    }

    // Create the session file before any update is published, so its record
    // starts with the session. Waits on a slot worker, never on the caller.
    fn start_recording(
        slot: &Arc<Slot>,
        record: Recording,
        name: &str,
        initialized: &Initialized,
    ) -> Result<Recorder, RpcError> {
        match record {
            Recording::Create { path, mut header } => {
                header.backend = Some(kage_session::entry::SessionBackend::ExternalAcp(
                    kage_session::entry::ExternalAcpSession {
                        agent_name: name.to_owned(),
                        protocol_version: initialized.protocol_version,
                        upstream_session_id: initialized.upstream.clone(),
                    },
                ));
                Recorder::start(slot, path, Target::Create(header), TIMEOUT)
            }
            Recording::Append { writer } => {
                let path = writer.path().to_path_buf();
                Recorder::start(slot, path, Target::Append(writer), TIMEOUT)
            }
        }
    }

    // Updates that arrived before the upstream session id are published and
    // recorded in arrival order once it is known, after the display history
    // of a restored session. Upstream history replay is discarded.
    fn publish(
        state: &Mutex<State>,
        id: &str,
        initialized: &Initialized,
        recorder: Option<Recorder>,
        seed: Vec<ExternalAcpPresentation>,
        history: Option<u64>,
    ) -> Result<Option<mpsc::Receiver<()>>, RpcError> {
        let mut state = lock(state);
        state.upstream = Some(initialized.upstream.clone());
        state.discard_replay = false;
        for event in seed {
            state.remember(event);
        }
        let written = match history {
            Some(owner) => Some(
                queue_history(&mut state, owner, id)
                    .ok_or_else(|| RpcError::internal("cannot send the session history"))?,
            ),
            None => None,
        };
        state.recorder = recorder;
        let early = std::mem::take(&mut state.early);
        for update in early {
            let value = update.value.clone();
            drop(update);
            forward(&mut state, id, value);
        }
        if state.closed {
            return Err(RpcError::internal(
                "external agent sent too many updates before session announcement",
            ));
        }
        Ok(written)
    }

    // A generic Peer cannot interrupt a write already in progress. Detaching
    // and terminal closure suppress queued updates but cannot guarantee that an
    // update already selected for writing will not arrive after the stop.
    // External sessions must remain disabled until the downstream transport
    // provides interruptible, ordered writes.
    //
    // A detached writer can still be blocked in a write, so writer threads are
    // counted until they exit rather than per attachment.
    fn attachment(
        peer: AgentPeer,
        slot: &Arc<Slot>,
        writers: &Arc<AtomicUsize>,
    ) -> Result<Attachment, RpcError> {
        if !reserve(writers, 1, MAX_WRITERS) {
            return Err(RpcError::internal("too many external attachment writers"));
        }
        let writer = Writer(Arc::clone(writers));
        let asks = peer.splits_requests();
        let (sender, receiver) = mpsc::sync_channel(1024);
        let detached = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&detached);
        #[cfg(test)]
        let (worker_done_tx, worker_done) = mpsc::channel();
        let spawned = slot.spawn("kage-external-writer", move || {
            let _writer = writer;
            while let Ok(item) = receiver.recv() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                match item {
                    WriteItem::Update(update) => {
                        if peer.notify("session/update", update.value.clone()).is_err() {
                            break;
                        }
                    }
                    WriteItem::Batch(updates) => {
                        if updates.iter().any(|update| {
                            stopped.load(Ordering::Acquire)
                                || peer.notify("session/update", update.value.clone()).is_err()
                        }) {
                            break;
                        }
                    }
                    WriteItem::Barrier(done) => {
                        let _ = done.send(());
                    }
                    // A dropped item tells the waiter that nothing was asked.
                    WriteItem::Ask {
                        params,
                        ask,
                        dispatched,
                    } => {
                        if ask.cancel.is_cancelled() {
                            continue;
                        }
                        match peer.dispatch_cancellable(
                            "session/request_permission",
                            params.value.clone(),
                            &ask.cancel,
                        ) {
                            Ok(request) => {
                                let _ = dispatched.send(request);
                            }
                            Err(_) if ask.cancel.is_cancelled() => {}
                            Err(_) => break,
                        }
                    }
                }
            }
            #[cfg(test)]
            let _ = worker_done_tx.send(());
        });
        spawned.map_err(|e| RpcError::internal(format!("cannot start external writer: {e}")))?;
        Ok(Attachment {
            sender: Some(sender),
            #[cfg(test)]
            worker_done: Some(worker_done),
            identity: Arc::new(()),
            epoch: 0,
            announced: false,
            asks,
            shown_history: false,
            receipts: 0,
            pending: Vec::new(),
            bytes: Arc::new(AtomicUsize::new(0)),
            detached,
        })
    }

    struct Writer(Arc<AtomicUsize>);

    impl Drop for Writer {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn forward(state: &mut State, id: &str, mut params: Value) {
        if state.closed {
            return;
        }
        if params.get("sessionId").and_then(Value::as_str) != state.upstream.as_deref() {
            return;
        }
        let item = format!("{}-{}", state.run, state.turn);
        for event in presentation(&params, &item) {
            state.record(event);
        }
        let Some(object) = params.as_object_mut() else {
            return;
        };
        object.insert("sessionId".to_owned(), Value::String(id.to_owned()));
        for attachment in state.attachments.values_mut() {
            let Some(update) = Buffered::new(params.clone(), &state.bytes, Some(&attachment.bytes))
            else {
                state.close();
                break;
            };
            if attachment.announced {
                if attachment
                    .sender
                    .as_ref()
                    .expect("open attachment")
                    .try_send(WriteItem::Update(update))
                    .is_err()
                {
                    state.close();
                    break;
                }
            } else {
                attachment.pending.push(update);
            }
        }
    }

    impl Drop for External {
        fn drop(&mut self) {
            self.terminate();
        }
    }

    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        use kage_acp::acp::StopReason;
        use std::collections::BTreeMap;
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixStream;

        const WAIT: Duration = Duration::from_secs(10);

        fn test_attachment(peer: AgentPeer) -> Attachment {
            static SLOT: std::sync::OnceLock<Arc<Slot>> = std::sync::OnceLock::new();
            let slot = SLOT.get_or_init(|| Lifecycles::new(1).reserve().unwrap().commit());
            attachment(peer, slot, &Arc::new(AtomicUsize::new(0))).unwrap()
        }

        fn response_barrier() -> (ResponseBarrier, kage_jsonrpc::BarrierMarker) {
            use std::io::Write;

            let (client, mut server) = UnixStream::pair().unwrap();
            let (peer, incoming, reader) =
                kage_jsonrpc::connect_ordered(BufReader::new(client.try_clone().unwrap()), client);
            let requester = thread::spawn(move || peer.request_ordered("test", Value::Null));
            let mut input = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            let id = serde_json::from_str::<Value>(&line).unwrap()["id"].clone();
            writeln!(
                server,
                "{}",
                json!({"jsonrpc": "2.0", "id": id, "result": true})
            )
            .unwrap();
            let (result, barrier) = requester.join().unwrap();
            assert_eq!(result.unwrap(), true);
            let kage_jsonrpc::OrderedInbound::Barrier(marker) = incoming.recv().unwrap() else {
                panic!("expected response barrier");
            };
            drop(server);
            drop(input);
            reader.join().unwrap();
            (barrier.unwrap(), marker)
        }

        #[test]
        fn ordered_result_requires_barrier_for_success_but_preserves_unbarriered_error() {
            let deadline = Instant::now() + Duration::from_secs(1);
            for operation in ["initialize", "session"] {
                assert_eq!(
                    ordered_result(Ok(()), None, deadline, operation).unwrap_err(),
                    RpcError::internal(format!("{operation} ordering barrier unavailable"))
                );
                let original = RpcError::new(-32042, "original error");
                assert_eq!(
                    ordered_result::<()>(Err(original.clone()), None, deadline, operation)
                        .unwrap_err(),
                    original
                );
            }
        }

        #[test]
        fn ordered_result_rejects_dropped_and_timed_out_barriers() {
            for is_error in [false, true] {
                for timeout in [false, true] {
                    let (barrier, marker) = response_barrier();
                    let marker = if timeout {
                        Some(marker)
                    } else {
                        drop(marker);
                        None
                    };
                    let result: Result<(), RpcError> = if is_error {
                        Err(RpcError::internal("original"))
                    } else {
                        Ok(())
                    };
                    let deadline = if timeout {
                        Instant::now()
                    } else {
                        Instant::now() + TIMEOUT
                    };
                    assert_eq!(
                        ordered_result(result, Some(barrier), deadline, "session").unwrap_err(),
                        RpcError::internal("session ordering barrier failed")
                    );
                    drop(marker);
                }
            }
        }

        #[test]
        fn ordered_result_waits_before_returning_success_or_error() {
            for is_error in [false, true] {
                let (barrier, marker) = response_barrier();
                let (started_tx, started_rx) = mpsc::channel();
                let (done_tx, done_rx) = mpsc::channel();
                let worker = thread::spawn(move || {
                    let result = if is_error {
                        Err(RpcError::internal("original"))
                    } else {
                        Ok(())
                    };
                    started_tx.send(()).unwrap();
                    done_tx
                        .send(ordered_result(
                            result,
                            Some(barrier),
                            Instant::now() + TIMEOUT,
                            "initialize",
                        ))
                        .unwrap();
                });
                started_rx.recv().unwrap();
                assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
                marker.acknowledge();
                let result = done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                if is_error {
                    assert_eq!(result.unwrap_err(), RpcError::internal("original"));
                } else {
                    result.unwrap();
                }
                worker.join().unwrap();
            }
        }

        struct GatedWriter {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            fail: bool,
            socket: Option<UnixStream>,
        }

        impl std::io::Write for GatedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let _ = self.entered.send(());
                let _ = self.release.recv();
                if self.fail {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "closed",
                    ))
                } else {
                    Ok(bytes.len())
                }
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        fn ordered_session(mut writer: GatedWriter) -> Arc<External> {
            let script = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'up','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'update'}}}}),flush=True)
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), script.into()],
                env: BTreeMap::default(),
            };
            let (a, b) = UnixStream::pair().unwrap();
            writer.socket = Some(a);
            let (peer, _, _) = kage_jsonrpc::connect(BufReader::new(b), writer);
            let cwd = std::env::current_dir().unwrap();
            let (session, created) = External::start(
                &Lifecycles::new(1),
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd: cwd.to_str().unwrap(),
                    config: &config,
                    owner: 1,
                    peer: peer.into(),
                    record: None,
                    restore: None,
                },
            )
            .unwrap();
            created.announce();
            session
        }

        #[test]
        fn prompt_waits_for_downstream_write_and_reports_disconnect() {
            for (fail, detach) in [(false, false), (true, false), (false, true)] {
                let (entered_tx, entered_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let session = ordered_session(GatedWriter {
                    entered: entered_tx,
                    release: release_rx,
                    fail,
                    socket: None,
                });
                let (done_tx, done_rx) = mpsc::channel();
                let active = Arc::clone(&session);
                let worker = thread::spawn(move || {
                    let result = active.prompt_owned(
                        1,
                        PromptRequest {
                            session_id: "local".into(),
                            prompt: vec![ContentBlock::text("hi")],
                            delivery: None,
                        },
                    );
                    done_tx.send(result).unwrap();
                });
                entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
                let detached_result = if detach {
                    assert!(session.detach(1));
                    Some(done_rx.recv_timeout(Duration::from_secs(3)).unwrap())
                } else {
                    None
                };
                release_tx.send(()).unwrap();
                let result = detached_result
                    .unwrap_or_else(|| done_rx.recv_timeout(Duration::from_secs(3)).unwrap());
                assert_eq!(result.is_err(), fail || detach);
                worker.join().unwrap();
                if !detach {
                    session.detach(1);
                }
            }
        }

        #[test]
        fn process_retains_context_and_routes_updates_after_announcement() {
            let script = r"
import json,sys,os
n=0
def send(obj):
 print(json.dumps(obj),flush=True)
def update(s, value):
 send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':s,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':value}}}})
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); p=m.get('params',{}); i=m.get('id')
 if method=='initialize': send({'jsonrpc':'2.0','id':i,'result':{'protocolVersion':1,'agentCapabilities':{}}})
 elif method=='session/new':
  update('up', 'created:'+os.getcwd()+':'+p['cwd'])
  send({'jsonrpc':'2.0','id':i,'result':{'sessionId':'up'}})
 elif method=='session/prompt':
  n+=1
  send({'jsonrpc':'2.0','id':'ask','method':'session/request_permission','params':{'sessionId':'up','toolCall':{},'options':[]}})
  answer=json.loads(sys.stdin.readline())
  update('up',str(n)+':'+answer['result']['outcome']['outcome'])
  send({'jsonrpc':'2.0','id':i,'result':{'stopReason':'end_turn'}})
";
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), script.into()],
                env: BTreeMap::default(),
            };
            let (client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let (peer, _incoming, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            let cwd = std::env::current_dir().unwrap();
            let cwd = cwd.to_str().unwrap();
            let (session, created) = External::start(
                &Lifecycles::new(1),
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd,
                    config: &config,
                    owner: 1,
                    peer: peer.into(),
                    record: None,
                    restore: None,
                },
            )
            .unwrap();
            created.announce();
            let mut client = BufReader::new(client);
            let mut line = String::new();
            client.read_line(&mut line).unwrap();
            let first: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(first["params"]["sessionId"], "local");
            assert_eq!(
                first["params"]["update"]["content"]["text"],
                format!("created:{cwd}:{cwd}")
            );
            for n in 1..=2 {
                let response = session
                    .prompt_owned(
                        1,
                        PromptRequest {
                            session_id: "local".into(),
                            prompt: vec![ContentBlock::text("hi")],
                            delivery: None,
                        },
                    )
                    .unwrap();
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                line.clear();
                client.read_line(&mut line).unwrap();
                let update: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(
                    update["params"]["update"]["content"]["text"],
                    format!("{n}:cancelled")
                );
            }
            assert!(session.detach(1));
        }
        fn scripted_session(script: &str) -> Arc<External> {
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), script.into()],
                env: BTreeMap::default(),
            };
            let (_a, b) = UnixStream::pair().unwrap();
            let (peer, _, _) = kage_jsonrpc::connect(BufReader::new(b.try_clone().unwrap()), b);
            let cwd = std::env::current_dir().unwrap();
            let (session, created) = External::start(
                &Lifecycles::new(1),
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd: cwd.to_str().unwrap(),
                    config: &config,
                    owner: 1,
                    peer: peer.into(),
                    record: None,
                    restore: None,
                },
            )
            .unwrap();
            created.announce();
            session
        }

        impl External {
            fn prompt_owned(
                &self,
                owner: u64,
                req: PromptRequest,
            ) -> Result<PromptResponse, RpcError> {
                let ticket = self.register_prompt(owner)?;
                self.prompt(ticket, req)
            }
        }

        fn request(text: &str) -> PromptRequest {
            PromptRequest {
                session_id: "local".into(),
                prompt: vec![ContentBlock::text(text)],
                delivery: None,
            }
        }

        #[test]
        fn cancel_during_blocked_dispatch_is_sent_after_commit() {
            let script = r"
import json,sys,time
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new':
  print(json.dumps({'jsonrpc':'2.0','id':i,'result':{'sessionId':'up'}}),flush=True)
  time.sleep(0.6)
  continue
 elif method=='session/prompt':
  cancel=json.loads(sys.stdin.readline())
  result={'stopReason':'cancelled' if cancel.get('method')=='session/cancel' else 'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let active = Arc::clone(&session);
            let worker =
                thread::spawn(move || active.prompt_owned(1, request(&"x".repeat(500_000))));
            let until = Instant::now() + Duration::from_secs(3);
            loop {
                let state = lock(&session.state);
                if state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.canceller.is_none())
                {
                    break;
                }
                drop(state);
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            session.cancel(1);
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert!(session.detach(1));
        }

        #[test]
        fn successor_waits_for_cancellation_write_completion() {
            let script = r"
import json,sys
n=0
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  n+=1
  if n==1:
   cancel=json.loads(sys.stdin.readline())
   if cancel.get('method')!='session/cancel': sys.exit(2)
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let active = Arc::clone(&session);
            let first = thread::spawn(move || active.prompt_owned(1, request("first")));
            let until = Instant::now() + Duration::from_secs(3);
            let action = loop {
                let mut state = lock(&session.state);
                if let Some(active) = state.active.as_mut()
                    && active.canceller.is_some()
                {
                    break active.cancel(&session.state).unwrap();
                }
                drop(state);
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            };
            let (_done, write_tx) = claim_cancel(&action).unwrap();
            action
                .canceller
                .cancel_if_active(Instant::now() + TIMEOUT, None)
                .unwrap();
            while lock(&session.state).active.is_some() {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            let ticket = session.register_prompt(1).unwrap();
            let queued = Arc::clone(&session);
            let (done_tx, done_rx) = mpsc::channel();
            let second = thread::spawn(move || {
                done_tx
                    .send(queued.prompt(ticket, request("second")))
                    .unwrap();
            });
            assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
            write_tx.send(Ok(())).unwrap();
            assert_eq!(
                first.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert_eq!(
                done_rx
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            second.join().unwrap();
            assert!(session.detach(1));
        }

        #[test]
        fn claimed_executor_scheduling_delay_does_not_close_session() {
            let dir = tempfile::tempdir().unwrap();
            let trigger = dir.path().join("release");
            let script = format!(
                r"
import json,sys,time,os
n=0
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={{'protocolVersion':1,'agentCapabilities':{{}}}}
 elif method=='session/new': result={{'sessionId':'up'}}
 elif method=='session/prompt':
  n+=1
  if n==1:
   while not os.path.exists({:?}): time.sleep(0.001)
  result={{'stopReason':'end_turn'}}
 elif method=='session/cancel': sys.exit(2)
 else: continue
 print(json.dumps({{'jsonrpc':'2.0','id':i,'result':result}}),flush=True)
",
                trigger.to_str().unwrap()
            );
            let session = scripted_session(&script);
            let active = Arc::clone(&session);
            let (first_tx, first_rx) = mpsc::channel();
            let first = thread::spawn(move || {
                let _ = first_tx.send(active.prompt_owned(1, request("first")));
            });
            let until = Instant::now() + Duration::from_secs(3);
            let action = loop {
                let mut state = lock(&session.state);
                if let Some(active) = state.active.as_mut()
                    && active.canceller.is_some()
                {
                    break active.cancel(&session.state).unwrap();
                }
                drop(state);
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            };
            let (_done, write_tx) = claim_cancel(&action).unwrap();
            std::fs::write(&trigger, "").unwrap();
            while lock(&session.state).active.is_some() {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            let queued = Arc::clone(&session);
            let (second_tx, second_rx) = mpsc::channel();
            let second = thread::spawn(move || {
                let _ = second_tx.send(queued.prompt_owned(1, request("second")));
            });
            assert!(matches!(
                first_rx.recv_timeout(TIMEOUT + Duration::from_millis(250)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            assert!(!lock(&session.state).closed);
            assert!(matches!(
                second_rx.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            let result = action
                .canceller
                .cancel_if_active(Instant::now() + TIMEOUT, None)
                .map(|_| ());
            write_tx.send(result).unwrap();
            assert_eq!(
                first_rx
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap()
                    .stop_reason,
                StopReason::Cancelled
            );
            assert_eq!(
                second_rx
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            first.join().unwrap();
            second.join().unwrap();
            assert!(!lock(&session.state).closed);
            assert!(session.detach(1));
        }

        #[test]
        fn shutdown_interrupts_claimed_cancellation_wait() {
            let script = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  cancel=json.loads(sys.stdin.readline())
  if cancel.get('method')!='session/cancel': sys.exit(2)
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let active = Arc::clone(&session);
            let (tx, rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let _ = tx.send(active.prompt_owned(1, request("first")));
            });
            let until = Instant::now() + Duration::from_secs(3);
            let action = loop {
                let mut state = lock(&session.state);
                if let Some(active) = state.active.as_mut()
                    && active.canceller.is_some()
                {
                    break active.cancel(&session.state).unwrap();
                }
                drop(state);
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            };
            let (_done, _write_tx) = claim_cancel(&action).unwrap();
            action
                .canceller
                .cancel_if_active(Instant::now() + TIMEOUT, None)
                .unwrap();
            while lock(&session.state).active.is_some() {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            session.terminate();
            assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().is_err());
            worker.join().unwrap();
            assert!(session.detach(1));
        }

        #[test]
        fn cancellation_write_error_closes_session() {
            let script = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  cancel=json.loads(sys.stdin.readline())
  if cancel.get('method')!='session/cancel': sys.exit(2)
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let active = Arc::clone(&session);
            let worker = thread::spawn(move || active.prompt_owned(1, request("first")));
            let until = Instant::now() + Duration::from_secs(3);
            let action = loop {
                let mut state = lock(&session.state);
                if let Some(active) = state.active.as_mut()
                    && active.canceller.is_some()
                {
                    break active.cancel(&session.state).unwrap();
                }
                drop(state);
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            };
            let (_done, write_tx) = claim_cancel(&action).unwrap();
            action
                .canceller
                .cancel_if_active(Instant::now() + TIMEOUT, None)
                .unwrap();
            write_tx
                .send(Err(RpcError::internal("uncertain write")))
                .unwrap();
            assert!(worker.join().unwrap().is_err());
            assert!(lock(&session.state).closed);
            assert!(session.register_prompt(1).is_err());
            assert!(session.detach(1));
        }

        #[test]
        fn delayed_cancel_after_prompt_completion_does_not_close_session() {
            let dir = tempfile::tempdir().unwrap();
            let trigger = dir.path().join("release");
            let script = r"
import json,sys,time,os,select
n=0
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  n+=1
  while not os.path.exists('TRIGGER'): time.sleep(0.001)
  if n==2 and select.select([sys.stdin],[],[],0.2)[0]:
   unexpected=json.loads(sys.stdin.readline())
   if unexpected.get('method')=='session/cancel': sys.exit(2)
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
"
            .replace("TRIGGER", trigger.to_str().unwrap());
            let session = scripted_session(&script);
            let active = Arc::clone(&session);
            let worker = thread::spawn(move || active.prompt_owned(1, request("first")));
            let until = Instant::now() + Duration::from_secs(3);
            let action = loop {
                let mut state = lock(&session.state);
                if let Some(active) = state.active.as_mut()
                    && active.canceller.is_some()
                {
                    break active.cancel(&session.state).unwrap();
                }
                drop(state);
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            };
            {
                let mut state = lock(&session.state);
                assert!(
                    state
                        .active
                        .as_mut()
                        .unwrap()
                        .cancel(&session.state)
                        .is_none()
                );
            }
            std::fs::write(&trigger, "").unwrap();
            while lock(&session.state).active.is_some() {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert!(!lock(&session.state).closed);
            std::fs::remove_file(&trigger).unwrap();
            let successor_session = Arc::clone(&session);
            let successor =
                thread::spawn(move || successor_session.prompt_owned(1, request("second")));
            let until = Instant::now() + Duration::from_secs(3);
            while lock(&session.state)
                .active
                .as_ref()
                .is_none_or(|active| active.canceller.is_none())
            {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            assert!(claim_cancel(&action).is_none());
            let (executor_tx, executor_rx) = mpsc::channel();
            let executor = thread::spawn(move || {
                run_cancel(&action);
                let _ = executor_tx.send(());
            });
            executor_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            executor.join().unwrap();
            std::fs::write(trigger, "").unwrap();
            assert_eq!(
                successor.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(!lock(&session.state).closed);
            assert!(session.detach(1));
        }

        #[test]
        fn cancellation_wins_over_an_upstream_success() {
            let script = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  cancel=json.loads(sys.stdin.readline())
  if cancel.get('method')!='session/cancel': sys.exit(2)
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let active = Arc::clone(&session);
            let worker = thread::spawn(move || active.prompt_owned(1, request("first")));
            let until = Instant::now() + Duration::from_secs(3);
            while lock(&session.state)
                .active
                .as_ref()
                .is_none_or(|active| active.canceller.is_none())
            {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            session.cancel(1);
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert!(session.detach(1));
        }

        #[test]
        fn stale_canceller_cannot_cancel_a_later_prompt() {
            let script = r"
import json,sys,time
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  time.sleep(0.15)
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let active = Arc::clone(&session);
            let first = thread::spawn(move || active.prompt_owned(1, request("first")));
            let until = Instant::now() + Duration::from_secs(3);
            let old = loop {
                if let Some(canceller) = lock(&session.state)
                    .active
                    .as_ref()
                    .and_then(|a| a.canceller.clone())
                {
                    break canceller;
                }
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            };
            assert_eq!(
                first.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            let active = Arc::clone(&session);
            let second = thread::spawn(move || active.prompt_owned(1, request("second")));
            assert!(
                old.cancel(Instant::now() + Duration::from_secs(1), None)
                    .is_err()
            );
            assert_eq!(
                second.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(session.detach(1));
        }

        #[test]
        fn shutdown_interrupts_stuck_dispatch_without_affecting_another_session() {
            let stuck = r"
import json,sys,time
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new':
  print(json.dumps({'jsonrpc':'2.0','id':i,'result':{'sessionId':'up'}}),flush=True)
  time.sleep(30)
  continue
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let healthy = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt': result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let blocked = scripted_session(stuck);
            let other = scripted_session(healthy);
            let active = Arc::clone(&blocked);
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(active.prompt_owned(1, request(&"x".repeat(500_000))));
            });
            let until = Instant::now() + Duration::from_secs(3);
            while lock(&blocked.state).active.is_none() {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            assert!(blocked.detach(1));
            assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().is_err());
            assert_eq!(
                other
                    .prompt_owned(1, request("still alive"))
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            assert!(other.detach(1));
        }

        #[test]
        fn disconnected_owner_cancels_prompt_with_another_attachment() {
            let script = r"
import json,sys
n=0
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  n+=1
  if n==1:
   cancel=json.loads(sys.stdin.readline())
   if cancel.get('method')!='session/cancel': sys.exit(2)
   result={'stopReason':'cancelled'}
  else: result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.into()).unwrap();
            session.announce(2);
            let active = Arc::clone(&session);
            let worker = thread::spawn(move || active.prompt_owned(1, request("first")));
            let until = Instant::now() + Duration::from_secs(3);
            while lock(&session.state)
                .active
                .as_ref()
                .is_none_or(|active| active.canceller.is_none())
            {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            assert!(!session.detach(1));
            assert!(worker.join().unwrap().is_err());
            assert_eq!(
                session
                    .prompt_owned(2, request("second"))
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            assert!(session.detach(2));
        }

        const ADMISSION_SCRIPT: &str = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt': result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";

        #[test]
        fn prompt_admission_is_bounded_and_released() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let other = scripted_session(ADMISSION_SCRIPT);
            let mut tickets: Vec<_> = (0..MAX_PROMPTS)
                .map(|_| session.register_prompt(1).unwrap())
                .collect();
            assert!(session.register_prompt(1).is_err());
            let independent = other.register_prompt(1).unwrap();
            drop(independent);

            drop(tickets.pop());
            let ticket = session.register_prompt(1).unwrap();
            let mut invalid = request("invalid");
            invalid.delivery = Some(PromptDelivery::Steer);
            assert!(session.prompt(ticket, invalid).is_err());
            let ticket = session.register_prompt(1).unwrap();
            assert_eq!(
                session.prompt(ticket, request("ok")).unwrap().stop_reason,
                StopReason::EndTurn
            );
            let ticket = session.register_prompt(1).unwrap();
            let panic = std::panic::catch_unwind(|| {
                let _ticket = ticket;
                panic!("test ticket unwind");
            });
            assert!(panic.is_err());
            let ticket = session.register_prompt(1).unwrap();
            drop(ticket);
            drop(tickets);
            assert_eq!(session.admitted.load(Ordering::Acquire), 0);
            assert!(session.detach(1));
            assert!(other.detach(1));
        }

        #[test]
        fn prompt_ticket_cannot_be_used_in_another_session() {
            let first = scripted_session(ADMISSION_SCRIPT);
            let second = scripted_session(ADMISSION_SCRIPT);
            let ticket = first.register_prompt(1).unwrap();
            assert!(second.prompt(ticket, request("wrong session")).is_err());
            assert_eq!(first.admitted.load(Ordering::Acquire), 0);
            assert_eq!(second.admitted.load(Ordering::Acquire), 0);
            assert!(first.detach(1));
            assert!(second.detach(1));
        }

        #[test]
        fn concurrent_registration_respects_limit() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let barrier = Arc::new(std::sync::Barrier::new(MAX_PROMPTS * 2));
            let workers: Vec<_> = (0..MAX_PROMPTS * 2)
                .map(|_| {
                    let session = Arc::clone(&session);
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        let ticket = session.register_prompt(1).ok();
                        barrier.wait();
                        ticket
                    })
                })
                .collect();
            let tickets: Vec<_> = workers
                .into_iter()
                .filter_map(|worker| worker.join().unwrap())
                .collect();
            assert_eq!(tickets.len(), MAX_PROMPTS);
            assert!(session.register_prompt(1).is_err());
            drop(tickets);
            assert_eq!(session.admitted.load(Ordering::Acquire), 0);
            assert!(session.detach(1));
        }

        #[test]
        fn active_prompt_occupies_an_admission_slot() {
            let dir = tempfile::tempdir().unwrap();
            let trigger = dir.path().join("release");
            let script = format!(
                r"
import json,sys,time,os
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={{'protocolVersion':1,'agentCapabilities':{{}}}}
 elif method=='session/new': result={{'sessionId':'up'}}
 elif method=='session/prompt':
  while not os.path.exists({:?}): time.sleep(0.001)
  result={{'stopReason':'end_turn'}}
 else: continue
 print(json.dumps({{'jsonrpc':'2.0','id':i,'result':result}}),flush=True)
",
                trigger.to_str().unwrap()
            );
            let session = scripted_session(&script);
            let worker_session = Arc::clone(&session);
            let ticket = session.register_prompt(1).unwrap();
            let worker = thread::spawn(move || worker_session.prompt(ticket, request("active")));
            let until = Instant::now() + Duration::from_secs(3);
            while lock(&session.state).active.is_none() {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            let tickets: Vec<_> = (1..MAX_PROMPTS)
                .map(|_| session.register_prompt(1).unwrap())
                .collect();
            assert!(session.register_prompt(1).is_err());
            std::fs::write(trigger, "release").unwrap();
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            let replacement = session.register_prompt(1).unwrap();
            drop(replacement);
            drop(tickets);
            assert!(session.detach(1));
        }

        #[test]
        fn cancelled_and_detached_waiters_release_admission() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.into()).unwrap();
            let gate = lock(&session.prompt_gate);
            let cancelled = session.register_prompt(1).unwrap();
            let detached = session.register_prompt(2).unwrap();
            let first = Arc::clone(&session);
            let second = Arc::clone(&session);
            let a = thread::spawn(move || first.prompt(cancelled, request("cancelled")));
            let b = thread::spawn(move || second.prompt(detached, request("detached")));
            session.cancel(1);
            assert!(!session.detach(2));
            assert_eq!(
                a.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert_eq!(
                b.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert_eq!(session.admitted.load(Ordering::Acquire), 0);
            let tickets: Vec<_> = (0..MAX_PROMPTS)
                .map(|_| session.register_prompt(1).unwrap())
                .collect();
            assert!(session.register_prompt(1).is_err());
            drop(tickets);
            drop(gate);
            assert!(session.detach(1));
        }

        #[test]
        fn cancellation_after_registration_invalidates_only_existing_tickets() {
            let script = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt': result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let gate = lock(&session.prompt_gate);
            let ticket = session.register_prompt(1).unwrap();
            let queued = Arc::clone(&session);
            let (tx, rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                tx.send(queued.prompt(ticket, request("cancelled")))
                    .unwrap();
            });
            session.cancel(1);
            let later = session.register_prompt(1).unwrap();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap()
                    .stop_reason,
                StopReason::Cancelled
            );
            drop(gate);
            worker.join().unwrap();
            assert_eq!(
                session.prompt(later, request("later")).unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(session.detach(1));
        }

        #[test]
        fn unannounced_saturation_reserves_a_slot_for_announced_owner() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.into()).unwrap();
            let tickets: Vec<_> = (0..MAX_PROMPTS - 1)
                .map(|_| session.register_prompt(2).unwrap())
                .collect();
            assert!(session.register_prompt(2).is_err());
            assert_eq!(
                session
                    .prompt_owned(1, request("other"))
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            drop(tickets);
            assert!(session.register_prompt(2).is_ok());
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        #[test]
        fn announcement_deadline_tie_times_out_but_preannounced_succeeds() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.into()).unwrap();
            let ticket = session.register_prompt(2).unwrap();
            assert!(
                session
                    .wait_for_announcement(&ticket, Duration::ZERO)
                    .is_err()
            );
            session.announce(2);
            assert!(
                session
                    .wait_for_announcement_until(&ticket, Instant::now())
                    .is_err()
            );
            assert!(
                session
                    .wait_for_announcement(&ticket, Duration::ZERO)
                    .unwrap()
            );
            drop(ticket);
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        #[test]
        fn unannounced_prompt_waits_without_blocking_other_attachments() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            let receipt = session.attach_open(2, peer.into()).unwrap();
            let ticket = session.register_prompt(2).unwrap();
            assert!(
                session
                    .wait_for_announcement(&ticket, Duration::ZERO)
                    .is_err()
            );
            let worker_session = Arc::clone(&session);
            let (tx, rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                tx.send(worker_session.prompt(ticket, request("waiting")))
                    .unwrap();
            });
            assert_eq!(
                session
                    .prompt_owned(1, request("other"))
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            receipt.announce();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            worker.join().unwrap();
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        #[test]
        fn unannounced_waiters_observe_cancellation_and_detach() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.clone().into()).unwrap();
            let cancelled = session.register_prompt(2).unwrap();
            let worker_session = Arc::clone(&session);
            let worker = thread::spawn(move || worker_session.prompt(cancelled, request("cancel")));
            session.cancel(2);
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );

            let detached = session.register_prompt(2).unwrap();
            let worker_session = Arc::clone(&session);
            let worker = thread::spawn(move || worker_session.prompt(detached, request("detach")));
            assert!(!session.detach(2));
            session.attach(2, peer.into()).unwrap();
            session.announce(2);
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert_eq!(session.admitted.load(Ordering::Acquire), 0);
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        fn attached(session: &External, owner: u64) -> bool {
            lock(&session.state).attachments.contains_key(&owner)
        }

        #[test]
        fn failed_open_detaches_only_an_unannounced_attachment_without_other_opens() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            let peer: AgentPeer = peer.into();

            let only = session.attach_open(2, peer.clone()).unwrap();
            assert_eq!(only.fail(), Some(false));
            assert!(!attached(&session, 2));

            let first = session.attach_open(2, peer.clone()).unwrap();
            let second = session.attach_open(2, peer.clone()).unwrap();
            drop(first);
            assert!(attached(&session, 2));
            drop(second);
            assert!(!attached(&session, 2));

            session.attach_open(2, peer.clone()).unwrap().announce();
            let late = session.attach_open(2, peer.clone()).unwrap();
            assert_eq!(late.fail(), None);
            assert!(attached(&session, 2));

            let stale = session.attach_open(3, peer.clone()).unwrap();
            assert!(!session.detach(3));
            let replacement = session.attach_open(3, peer.clone()).unwrap();
            assert_eq!(stale.fail(), None);
            assert!(attached(&session, 3));
            assert_eq!(replacement.fail(), Some(false));
            assert!(!attached(&session, 3));
            assert!(!lock(&session.state).closed);
        }

        #[test]
        fn failed_creation_response_closes_the_new_session() {
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), ADMISSION_SCRIPT.into()],
                env: BTreeMap::default(),
            };
            let (_a, b) = UnixStream::pair().unwrap();
            let (peer, _, _) = kage_jsonrpc::connect(BufReader::new(b.try_clone().unwrap()), b);
            let cwd = std::env::current_dir().unwrap();
            let lifecycles = Lifecycles::new(1);
            let (session, created) = External::start(
                &lifecycles,
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd: cwd.to_str().unwrap(),
                    config: &config,
                    owner: 1,
                    peer: peer.into(),
                    record: None,
                    restore: None,
                },
            )
            .unwrap();
            assert_eq!(created.fail(), Some(true));
            assert!(lock(&session.state).closed);
            assert!(session.slot.wait_finished(WAIT));
            assert_eq!(lifecycles.live(), 0);
        }

        #[test]
        fn pending_opens_are_bounded_per_attachment() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            let peer: AgentPeer = peer.into();
            let mut opens: Vec<_> = (0..MAX_OPEN_RECEIPTS)
                .map(|_| session.attach_open(2, peer.clone()).unwrap())
                .collect();
            assert!(session.attach_open(2, peer.clone()).is_err());
            opens.pop().unwrap().announce();
            assert!(session.attach_open(2, peer).is_ok());
            drop(opens);
            assert!(attached(&session, 2));
        }

        #[test]
        fn failed_open_of_last_attachment_closes_and_cleans_up_session() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            let open = session.attach_open(2, peer.into()).unwrap();
            assert!(!session.detach(1));
            assert_eq!(open.fail(), Some(true));
            assert!(lock(&session.state).closed);
            assert!(session.slot.wait_finished(WAIT));
        }

        // Asks once per prompt and reports the outcome it received as text.
        // Answers are buffered by id, since they can arrive in any order and
        // after a cancellation notice.
        const ASK_SCRIPT: &str = r"
import json,sys
def send(m): print(json.dumps(m),flush=True)
seen={}
def answer(i):
 while i not in seen:
  r=json.loads(sys.stdin.readline())
  if 'id' in r and 'method' not in r: seen[r['id']]=r.get('result',{}).get('outcome',{})
 return seen.pop(i)
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  n=len(m['params']['prompt'][0]['text'])
  for k in range(n):
   send({'jsonrpc':'2.0','id':'p%d'%k,'method':'session/request_permission','params':{'sessionId':'up','toolCall':{'toolCallId':'c1'},'options':[{'optionId':'yes','name':'Yes','kind':'allow_once'},{'optionId':'no','name':'No','kind':'reject_once'}],'_meta':{'x':1}}})
  got={}
  for k in reversed(range(n)):
   o=answer('p%d'%k); got[k]=o.get('outcome','')+':'+o.get('optionId','')
   if k==n-1 and n>1:
    send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'up','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'last '+got[k]}}}})
  text=' '.join(got[k] for k in range(n))
  send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'up','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':text}}}})
  result={'stopReason':'end_turn'}
 else: continue
 send({'jsonrpc':'2.0','id':i,'result':result})
";

        // A downstream client on an ordered Unix transport, which forwarded
        // asks need. Barriers are acknowledged as they arrive.
        fn unix_client() -> (
            AgentPeer,
            kage_jsonrpc::Peer,
            mpsc::Receiver<kage_jsonrpc::Inbound>,
        ) {
            let (reader, input) = std::io::pipe().unwrap();
            let (output, writer) = std::io::pipe().unwrap();
            let (unix, inbound, _reader) = kage_jsonrpc::unix_peer::connect_ordered_server(
                BufReader::new(reader),
                writer.into(),
            )
            .unwrap();
            thread::spawn(move || {
                for item in inbound {
                    if let kage_jsonrpc::OrderedInbound::Barrier(marker) = item {
                        marker.acknowledge();
                    }
                }
            });
            let (client, inbox, _reader) = kage_jsonrpc::connect(BufReader::new(output), input);
            (AgentPeer::unix(unix, Duration::from_secs(2)), client, inbox)
        }

        fn ask_session(peer: AgentPeer) -> Arc<External> {
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), ASK_SCRIPT.into()],
                env: BTreeMap::default(),
            };
            let cwd = std::env::current_dir().unwrap();
            let (session, created) = External::start(
                &Lifecycles::new(1),
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd: cwd.to_str().unwrap(),
                    config: &config,
                    owner: 1,
                    peer,
                    record: None,
                    restore: None,
                },
            )
            .unwrap();
            created.announce();
            session
        }

        fn next_request(inbox: &mpsc::Receiver<kage_jsonrpc::Inbound>) -> (Value, Value) {
            loop {
                match inbox.recv_timeout(WAIT).unwrap() {
                    kage_jsonrpc::Inbound::Request { id, method, params } => {
                        assert_eq!(method, "session/request_permission");
                        break (id, params);
                    }
                    kage_jsonrpc::Inbound::Notification { .. } => {}
                }
            }
        }

        fn next_text(inbox: &mpsc::Receiver<kage_jsonrpc::Inbound>) -> String {
            loop {
                if let kage_jsonrpc::Inbound::Notification { method, params } =
                    inbox.recv_timeout(WAIT).unwrap()
                    && method == "session/update"
                {
                    break params["update"]["content"]["text"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                }
            }
        }

        fn prompt_in_background(
            session: &Arc<External>,
            text: &str,
        ) -> thread::JoinHandle<Result<PromptResponse, RpcError>> {
            let session = Arc::clone(session);
            let text = text.to_owned();
            thread::spawn(move || session.prompt_owned(1, request(&text)))
        }

        #[test]
        fn permission_ask_reaches_the_prompt_owner_and_returns_its_selection() {
            let (peer, client, inbox) = unix_client();
            let session = ask_session(peer);
            let (_other_client, other_server) = UnixStream::pair().unwrap();
            let (other, _, _) = kage_jsonrpc::connect(
                BufReader::new(other_server.try_clone().unwrap()),
                other_server,
            );
            session.attach(2, other.into()).unwrap();
            session.announce(2);
            let prompt = prompt_in_background(&session, "a");
            let (id, params) = next_request(&inbox);
            assert_eq!(params["sessionId"], "local");
            assert_eq!(params["_meta"], json!({"x": 1}));
            assert_eq!(params["options"][1]["optionId"], "no");
            client
                .respond(
                    &id,
                    Ok(json!({"outcome": {"outcome": "selected", "optionId": "yes"}})),
                )
                .unwrap();
            assert_eq!(next_text(&inbox), "selected:yes");
            assert_eq!(
                prompt.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            let until = Instant::now() + WAIT;
            while !lock(&session.state).asks.is_empty() {
                assert!(Instant::now() < until, "answered ask was not released");
                thread::sleep(Duration::from_millis(2));
            }
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        #[test]
        fn unoffered_selection_is_answered_as_cancelled() {
            let (peer, client, inbox) = unix_client();
            let session = ask_session(peer);
            let prompt = prompt_in_background(&session, "a");
            let (id, _) = next_request(&inbox);
            client
                .respond(
                    &id,
                    Ok(json!({"outcome": {"outcome": "selected", "optionId": "always"}})),
                )
                .unwrap();
            assert_eq!(next_text(&inbox), "cancelled:");
            assert_eq!(
                prompt.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(session.detach(1));
        }

        #[test]
        fn cancelling_the_prompt_withdraws_its_ask() {
            let (peer, client, inbox) = unix_client();
            let session = ask_session(peer);
            let prompt = prompt_in_background(&session, "a");
            let (id, _) = next_request(&inbox);
            session.cancel(1);
            loop {
                if let kage_jsonrpc::Inbound::Notification { method, params } =
                    inbox.recv_timeout(WAIT).unwrap()
                    && method == "$/cancel_request"
                {
                    assert_eq!(params["requestId"], id);
                    break;
                }
            }
            client
                .respond(
                    &id,
                    Ok(json!({"outcome": {"outcome": "selected", "optionId": "yes"}})),
                )
                .unwrap();
            assert_eq!(next_text(&inbox), "cancelled:");
            assert_eq!(
                prompt.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert!(session.detach(1));
        }

        #[test]
        fn asks_for_a_generic_downstream_peer_are_cancelled() {
            let (client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            let session = ask_session(peer.into());
            let prompt = prompt_in_background(&session, "a");
            let mut lines = BufReader::new(client).lines();
            let update = loop {
                let line: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                assert_ne!(line["method"], "session/request_permission");
                if line["method"] == "session/update" {
                    break line;
                }
            };
            assert_eq!(update["params"]["update"]["content"]["text"], "cancelled:");
            assert_eq!(
                prompt.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(session.detach(1));
        }

        #[test]
        fn outstanding_asks_are_bounded() {
            let (peer, client, inbox) = unix_client();
            let session = ask_session(peer);
            let prompt = prompt_in_background(&session, &"a".repeat(MAX_ASKS + 1));
            let ids: Vec<_> = (0..MAX_ASKS).map(|_| next_request(&inbox).0).collect();
            assert_eq!(next_text(&inbox), "last cancelled:");
            for id in &ids {
                client
                    .respond(
                        id,
                        Ok(json!({"outcome": {"outcome": "selected", "optionId": "no"}})),
                    )
                    .unwrap();
            }
            let expected = format!("{}cancelled:", "selected:no ".repeat(MAX_ASKS));
            assert_eq!(next_text(&inbox), expected);
            assert_eq!(
                prompt.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(session.detach(1));
        }

        // Streams one update during each prompt. A prompt saying "hang" gets
        // no response.
        const RECORD_SCRIPT: &str = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'up','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'reply'}}}}),flush=True)
  if m['params']['prompt'][0]['text']=='hang': continue
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";

        fn recorded_session(
            dir: &std::path::Path,
        ) -> (Arc<External>, std::path::PathBuf, UnixStream) {
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), RECORD_SCRIPT.into()],
                env: BTreeMap::default(),
            };
            let (client, b) = UnixStream::pair().unwrap();
            let (peer, _, _) = kage_jsonrpc::connect(BufReader::new(b.try_clone().unwrap()), b);
            let (path, mut header) = crate::plan_session_in(dir, "external", "");
            header.cwd = dir.to_path_buf();
            let (session, created) = External::start(
                &Lifecycles::new(1),
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd: dir.to_str().unwrap(),
                    config: &config,
                    owner: 1,
                    peer: peer.into(),
                    record: Some(Recording::Create {
                        path: path.clone(),
                        header: Box::new(header),
                    }),
                    restore: None,
                },
            )
            .unwrap();
            created.announce();
            (session, path, client)
        }

        fn shown(event: &ExternalAcpPresentation) -> String {
            match event {
                ExternalAcpPresentation::Prompt { text } => format!("prompt {}", text.as_str()),
                ExternalAcpPresentation::AgentMessage { text, .. } => {
                    format!("message {}", text.as_str())
                }
                ExternalAcpPresentation::TurnEnd { outcome } => format!("end {outcome:?}"),
                other => format!("{other:?}"),
            }
        }

        #[test]
        fn turns_are_recorded_in_order_with_the_upstream_identity() {
            let dir = tempfile::tempdir().unwrap();
            let (session, path, _client) = recorded_session(dir.path());
            for text in ["one", "two"] {
                assert_eq!(
                    session.prompt_owned(1, request(text)).unwrap().stop_reason,
                    StopReason::EndTurn
                );
            }
            assert!(session.detach(1));
            assert!(session.slot.wait_finished(WAIT));
            let replay = kage_session::external_replay::replay_external(&path).unwrap();
            let Some(kage_session::entry::SessionBackend::ExternalAcp(backend)) =
                &replay.header.backend
            else {
                panic!("missing external backend");
            };
            assert_eq!(backend.upstream_session_id, "up");
            assert_eq!(backend.agent_name, "fake");
            let events: Vec<_> = replay.events.iter().map(shown).collect();
            assert_eq!(
                events,
                [
                    "prompt one",
                    "message reply",
                    "end EndTurn",
                    "prompt two",
                    "message reply",
                    "end EndTurn"
                ]
            );
        }

        #[test]
        fn closing_during_a_turn_records_it_as_failed() {
            let dir = tempfile::tempdir().unwrap();
            let (session, path, _client) = recorded_session(dir.path());
            let active = Arc::clone(&session);
            let prompt = thread::spawn(move || active.prompt_owned(1, request("hang")));
            let until = Instant::now() + WAIT;
            while lock(&session.state).active.is_none() {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            assert!(session.detach(1));
            assert!(prompt.join().unwrap().is_err());
            assert!(session.slot.wait_finished(WAIT));
            let replay = kage_session::external_replay::replay_external(&path).unwrap();
            let events: Vec<_> = replay.events.iter().map(shown).collect();
            assert_eq!(events.first().map(String::as_str), Some("prompt hang"));
            assert_eq!(events.last().map(String::as_str), Some("end Failed"));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.starts_with("end"))
                    .count(),
                1
            );
        }

        // Advertises the restore capability named by its argument and fails if
        // asked to create a session. A load replays one history update.
        const RESTORE_SCRIPT: &str = r"
import json,sys
caps={'resume':{'sessionCapabilities':{'resume':{}}},'load':{'loadSession':True},'none':{}}[sys.argv[1]]
used=None
def update(sid,text): print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':sid,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':text}}}}),flush=True)
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id'); p=m.get('params',{})
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':caps}
 elif method=='session/new': sys.exit(3)
 elif method in ('session/resume','session/load'):
  used=method
  if method=='session/load': update(p['sessionId'],'history')
  result={}
 elif method=='session/prompt':
  update(p['sessionId'],used+' '+p['sessionId'])
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";

        type Restored = (
            Result<(Arc<External>, OpenTicket), RpcError>,
            Lifecycles,
            std::path::PathBuf,
            UnixStream,
        );

        fn restored_session(dir: &std::path::Path, capability: &str, replay: bool) -> Restored {
            let (path, mut header) = crate::plan_session_in(dir, "external", "");
            header.cwd = dir.to_path_buf();
            header.backend = Some(kage_session::entry::SessionBackend::ExternalAcp(
                kage_session::entry::ExternalAcpSession {
                    agent_name: "fake".into(),
                    protocol_version: 1,
                    upstream_session_id: "up-old".into(),
                },
            ));
            let mut writer = kage_session::SessionWriter::create(&path, header).unwrap();
            for event in [
                ExternalAcpPresentation::Prompt {
                    text: bounded("before"),
                },
                ExternalAcpPresentation::AgentMessage {
                    item_id: bounded("old-1"),
                    text: bounded("earlier"),
                },
                ExternalAcpPresentation::TurnEnd {
                    outcome: PresentationTurnOutcome::EndTurn,
                },
            ] {
                writer
                    .append(
                        &event.into_entry(kage_session::entry::EntryId::new(), chrono::Utc::now()),
                    )
                    .unwrap();
            }
            drop(writer);
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec![
                    "-u".into(),
                    "-c".into(),
                    RESTORE_SCRIPT.into(),
                    capability.into(),
                ],
                env: BTreeMap::default(),
            };
            let (client, b) = UnixStream::pair().unwrap();
            let (peer, _, _) = kage_jsonrpc::connect(BufReader::new(b.try_clone().unwrap()), b);
            let lifecycles = Lifecycles::new(1);
            let started = External::start(
                &lifecycles,
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd: dir.to_str().unwrap(),
                    config: &config,
                    owner: 1,
                    peer: peer.into(),
                    record: Some(Recording::Append {
                        writer: Box::new(kage_session::SessionWriter::open(&path).unwrap()),
                    }),
                    restore: Some(Restore {
                        upstream: "up-old".into(),
                        history: replay,
                    }),
                },
            );
            (started, lifecycles, path, client)
        }

        fn recorded_texts(path: &std::path::Path) -> Vec<String> {
            kage_session::external_replay::replay_external(path)
                .unwrap()
                .events
                .iter()
                .map(shown)
                .collect()
        }

        #[test]
        fn restoring_resumes_the_recorded_upstream_session_and_appends() {
            let dir = tempfile::tempdir().unwrap();
            let (started, _lifecycles, path, client) =
                restored_session(dir.path(), "resume", false);
            let (session, created) = started.unwrap();
            created.announce();
            let texts = client_texts(client);
            assert_eq!(
                session
                    .prompt_owned(1, request("again"))
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            assert!(session.detach(1));
            assert!(session.slot.wait_finished(WAIT));
            assert_eq!(
                texts.recv_timeout(WAIT).unwrap().1,
                "session/resume up-old",
                "a resume must not send history"
            );
            assert_eq!(
                recorded_texts(&path),
                [
                    "prompt before",
                    "message earlier",
                    "end EndTurn",
                    "prompt again",
                    "message session/resume up-old",
                    "end EndTurn"
                ]
            );
        }

        // Update texts the client receives, read without blocking the test.
        fn client_texts(client: UnixStream) -> mpsc::Receiver<(String, String)> {
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(client).lines() {
                    let Ok(line) = line else { break };
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let update = &value["params"]["update"];
                    if let (Some(kind), Some(text)) = (
                        update["sessionUpdate"].as_str(),
                        update["content"]["text"].as_str(),
                    ) && tx.send((kind.to_owned(), text.to_owned())).is_err()
                    {
                        break;
                    }
                }
            });
            rx
        }

        #[test]
        fn a_long_history_is_cut_to_its_newest_part_without_closing_the_session() {
            let session = scripted_session(ADMISSION_SCRIPT);
            {
                let mut state = lock(&session.state);
                for n in 0..5000 {
                    state.remember(ExternalAcpPresentation::AgentMessage {
                        item_id: bounded(&format!("item-{n}")),
                        text: bounded(&format!("message {n}")),
                    });
                }
            }
            let (client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach_load(2, peer.into()).unwrap().announce();
            let texts = client_texts(client);
            let first = texts.recv_timeout(WAIT).unwrap();
            assert!(first.1.contains("not recorded"), "{first:?}");
            let mut last = first;
            while let Ok(text) = texts.recv_timeout(Duration::from_millis(500)) {
                last = text;
            }
            assert_eq!(last.1, "message 4999");
            let mut state = lock(&session.state);
            assert!(!state.closed);
            forward(
                &mut state,
                "local",
                json!({"sessionId": "up", "update": {"sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "live"}}}),
            );
            assert!(
                !state.closed,
                "a live update after the history must still fit"
            );
            drop(state);
            assert_eq!(texts.recv_timeout(WAIT).unwrap().1, "live");
            assert!(session.attach_load(2, peer_for_test()).is_err());
        }

        #[test]
        fn a_load_is_refused_while_another_open_of_the_attachment_is_unresolved() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let pending = session.attach_open(2, peer_for_test()).unwrap();
            assert!(session.attach_load(2, peer_for_test()).is_err());
            pending.announce();
            assert!(session.attach_load(2, peer_for_test()).is_err());
            assert!(!session.detach(2));
            let mut load = session.attach_load(2, peer_for_test()).unwrap();
            load.wait_history().unwrap();
            load.announce();
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        fn peer_for_test() -> AgentPeer {
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            peer.into()
        }

        #[test]
        fn a_load_shows_local_history_and_discards_the_upstream_replay() {
            let dir = tempfile::tempdir().unwrap();
            let (started, _lifecycles, path, client) = restored_session(dir.path(), "load", true);
            let (session, created) = started.unwrap();
            created.announce();
            let texts = client_texts(client);
            assert_eq!(
                session
                    .prompt_owned(1, request("again"))
                    .unwrap()
                    .stop_reason,
                StopReason::EndTurn
            );
            let seen: Vec<_> = (0..3).map(|_| texts.recv_timeout(WAIT).unwrap()).collect();
            assert_eq!(
                seen,
                [
                    ("user_message_chunk".into(), "before".into()),
                    ("agent_message_chunk".into(), "earlier".into()),
                    ("agent_message_chunk".into(), "session/load up-old".into()),
                ]
            );
            assert!(session.detach(1));
            assert!(session.slot.wait_finished(WAIT));
            assert_eq!(
                recorded_texts(&path),
                [
                    "prompt before",
                    "message earlier",
                    "end EndTurn",
                    "prompt again",
                    "message session/load up-old",
                    "end EndTurn"
                ]
            );
        }

        #[test]
        fn an_agent_that_cannot_restore_is_refused_without_a_new_session() {
            let dir = tempfile::tempdir().unwrap();
            let (started, lifecycles, path, _client) = restored_session(dir.path(), "none", true);
            let Err(error) = started else {
                panic!("restoration must fail");
            };
            assert_eq!(error.code, -32602, "{error:?}");
            let until = Instant::now() + WAIT;
            while lifecycles.live() != 0 {
                assert!(Instant::now() < until, "slot was not released");
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(recorded_texts(&path).len(), 3);
        }

        #[test]
        fn open_receipts_are_fresh_exact_once_and_attachment_scoped() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            let first = session.attach_open(2, peer.clone().into()).unwrap();
            let second = session.attach_open(2, peer.clone().into()).unwrap();
            let dropped = session.attach_open(2, peer.clone().into()).unwrap();
            drop(dropped);
            let (sender, receiver) = mpsc::sync_channel(4);
            {
                let mut state = lock(&session.state);
                state.attachments.get_mut(&2).unwrap().sender = Some(sender);
                forward(&mut state, "local", json!({"sessionId":"up", "value":1}));
                assert!(!state.attachments[&2].announced);
                assert_eq!(state.attachments[&2].pending.len(), 1);
            }
            second.announce();
            first.announce();
            assert!(
                matches!(receiver.try_recv(), Ok(WriteItem::Batch(updates)) if updates.len() == 1)
            );
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));

            let stale = session.attach_open(2, peer.clone().into()).unwrap();
            assert!(!session.detach(2));
            let replacement = session.attach_open(2, peer.into()).unwrap();
            stale.announce();
            assert!(!lock(&session.state).attachments[&2].announced);
            replacement.announce();
            assert!(lock(&session.state).attachments[&2].announced);
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        #[test]
        fn registered_ticket_is_cancelled_by_detach_and_reattach() {
            let script = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt': result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.clone().into()).unwrap();
            session.announce(2);
            let gate = lock(&session.prompt_gate);
            let ticket = session.register_prompt(2).unwrap();
            assert!(!session.detach(2));
            session.attach(2, peer.into()).unwrap();
            session.announce(2);
            drop(gate);
            assert_eq!(
                session
                    .prompt(ticket, request("stale"))
                    .unwrap()
                    .stop_reason,
                StopReason::Cancelled
            );
            assert_eq!(
                session.prompt_owned(2, request("new")).unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        #[test]
        fn stale_completion_never_enqueues_on_replacement() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.clone().into()).unwrap();
            let ticket = session.register_prompt(2).unwrap();
            assert!(!session.detach(2));
            session.attach(2, peer.into()).unwrap();
            let (sender, receiver) = mpsc::sync_channel(1);
            {
                let mut state = lock(&session.state);
                let attachment = state.attachments.get_mut(&2).unwrap();
                attachment.sender = Some(sender);
                attachment.announced = true;
            }
            assert!(session.wait_for_writes(&ticket).is_err());
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert!(!session.detach(2));
            assert!(session.detach(1));
        }

        #[test]
        fn detached_barrier_disconnect_does_not_close_shared_session() {
            for detach in [true, false] {
                let session = scripted_session(ADMISSION_SCRIPT);
                let (_client, server) = UnixStream::pair().unwrap();
                let (peer, _, _) =
                    kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
                session.attach(2, peer.into()).unwrap();
                let ticket = session.register_prompt(2).unwrap();
                let (sender, receiver) = mpsc::sync_channel(1);
                {
                    let mut state = lock(&session.state);
                    let attachment = state.attachments.get_mut(&2).unwrap();
                    attachment.sender = Some(sender);
                    attachment.announced = true;
                }
                let worker_session = Arc::clone(&session);
                let worker = thread::spawn(move || worker_session.wait_for_writes(&ticket));
                let barrier = receiver.recv_timeout(Duration::from_secs(3)).unwrap();
                if detach {
                    assert!(!session.detach(2));
                } else {
                    session.cancel(2);
                }
                drop(barrier);
                assert!(worker.join().unwrap().is_err());
                assert_eq!(lock(&session.state).closed, !detach);
                if detach {
                    assert_eq!(
                        session
                            .prompt_owned(1, request("still alive"))
                            .unwrap()
                            .stop_reason,
                        StopReason::EndTurn
                    );
                } else {
                    assert!(!session.detach(2));
                }
                assert!(session.detach(1));
            }
        }

        #[test]
        fn cancel_after_upstream_completion_does_not_break_barrier() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (sender, receiver) = mpsc::sync_channel(1);
            lock(&session.state).attachments.get_mut(&1).unwrap().sender = Some(sender);
            let active = Arc::clone(&session);
            let worker = thread::spawn(move || active.prompt_owned(1, request("complete")));
            let WriteItem::Barrier(ack) = receiver.recv_timeout(Duration::from_secs(3)).unwrap()
            else {
                panic!("expected completion barrier");
            };
            assert!(lock(&session.state).active.is_none());
            session.cancel(1);
            ack.send(()).unwrap();
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(!lock(&session.state).closed);
            assert!(session.detach(1));
        }

        #[test]
        fn repeated_cancellation_preserves_active_cancelled_result() {
            let dir = tempfile::tempdir().unwrap();
            let trigger = dir.path().join("release");
            let script = format!(
                r"
import json,sys,time,os
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={{'protocolVersion':1,'agentCapabilities':{{}}}}
 elif method=='session/new': result={{'sessionId':'up'}}
 elif method=='session/prompt':
  while not os.path.exists({:?}): time.sleep(0.001)
  result={{'stopReason':'end_turn'}}
 else: continue
 print(json.dumps({{'jsonrpc':'2.0','id':i,'result':result}}),flush=True)
",
                trigger.to_str().unwrap()
            );
            let session = scripted_session(&script);
            let worker_session = Arc::clone(&session);
            let worker = thread::spawn(move || worker_session.prompt_owned(1, request("active")));
            let until = Instant::now() + Duration::from_secs(3);
            while lock(&session.state)
                .active
                .as_ref()
                .is_none_or(|active| active.canceller.is_none())
            {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            session.cancel(1);
            session.cancel(1);
            std::fs::write(trigger, "release").unwrap();
            assert_eq!(
                worker.join().unwrap().unwrap().stop_reason,
                StopReason::Cancelled
            );
            assert!(!lock(&session.state).closed);
            assert!(session.detach(1));
        }

        #[test]
        fn replacement_cannot_cancel_old_active_prompt() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.clone().into()).unwrap();
            let ticket = session.register_prompt(2).unwrap();
            assert!(!session.detach(2));
            session.attach(2, peer.into()).unwrap();
            {
                let mut state = lock(&session.state);
                state.active = Some(Active {
                    owner: 2,
                    identity: Arc::clone(&ticket.identity),
                    token: Arc::new(()),
                    cancelled: false,
                    canceller: None,
                    abort: CancelFlag::new(),
                    cancellation: Cancellation::None,
                });
            }
            session.cancel(2);
            assert!(!lock(&session.state).active.as_ref().unwrap().cancelled);
            assert!(!session.detach(2));
            assert!(!lock(&session.state).active.take().unwrap().cancelled);
            assert!(session.detach(1));
        }

        #[test]
        fn repeated_owner_churn_leaves_only_live_attachments() {
            let session = scripted_session(ADMISSION_SCRIPT);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            for owner in 2..258 {
                session.attach(owner, peer.clone().into()).unwrap();
                session.cancel(owner);
                assert!(!session.detach(owner));
                assert_eq!(lock(&session.state).attachments.len(), 1);
                let writers = Arc::clone(&lock(&session.state).writers);
                let until = Instant::now() + WAIT;
                while writers.load(Ordering::Acquire) > 1 {
                    assert!(Instant::now() < until, "detached writer did not exit");
                    thread::sleep(Duration::from_millis(1));
                }
            }
            assert!(session.detach(1));
        }

        #[test]
        fn buffer_budgets_are_released_and_overflow_fails_closed() {
            let session_bytes = Arc::new(AtomicUsize::new(0));
            let attachment_bytes = Arc::new(AtomicUsize::new(0));
            let large = Value::String("x".repeat(ATTACHMENT_BUDGET));
            assert!(Buffered::new(large, &session_bytes, Some(&attachment_bytes)).is_none());
            assert_eq!(session_bytes.load(Ordering::Acquire), 0);
            let update = Buffered::new(
                json!({"text": "hello"}),
                &session_bytes,
                Some(&attachment_bytes),
            )
            .unwrap();
            assert!(session_bytes.load(Ordering::Acquire) > 0);
            assert!(attachment_bytes.load(Ordering::Acquire) > 0);
            drop(update);
            assert_eq!(session_bytes.load(Ordering::Acquire), 0);
            assert_eq!(attachment_bytes.load(Ordering::Acquire), 0);

            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            let mut state = State {
                upstream: Some("up".into()),
                early: Vec::new(),
                bytes: session_bytes,
                attachments: HashMap::from([(1, test_attachment(peer.into()))]),
                active: None,
                closed: false,
                cancels: None,
                writers: Arc::new(AtomicUsize::new(0)),
                asks: Vec::new(),
                recorder: None,
                history: std::collections::VecDeque::new(),
                history_bytes: 0,
                history_cut: false,
                run: kage_session::entry::EntryId::new().to_string(),
                turn: 0,
                turn_open: false,
                discard_replay: false,
            };
            forward(
                &mut state,
                "local",
                json!({"sessionId":"up", "text":"x".repeat(ATTACHMENT_BUDGET)}),
            );
            assert!(state.closed);
            assert!(state.attachments[&1].pending.is_empty());
        }

        #[test]
        fn detached_attachment_discards_queued_updates() {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (client, server) = UnixStream::pair().unwrap();
            let writer = GatedWriter {
                entered: entered_tx,
                release: release_rx,
                fail: false,
                socket: Some(client),
            };
            let (peer, _, _) = kage_jsonrpc::connect(BufReader::new(server), writer);
            let attachment = test_attachment(peer.into());
            let bytes = Arc::new(AtomicUsize::new(0));
            let item = || {
                Buffered::new(json!({"text":"update"}), &bytes, Some(&attachment.bytes)).unwrap()
            };
            assert!(
                attachment
                    .sender
                    .as_ref()
                    .unwrap()
                    .send(WriteItem::Update(item()))
                    .is_ok()
            );
            entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(
                attachment
                    .sender
                    .as_ref()
                    .unwrap()
                    .send(WriteItem::Update(item()))
                    .is_ok()
            );
            attachment.detached.store(true, Ordering::Release);
            release_tx.send(()).unwrap();
            assert!(entered_rx.recv_timeout(Duration::from_millis(200)).is_err());
        }

        #[test]
        fn terminal_close_discards_queued_updates_and_barriers() {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (client, server) = UnixStream::pair().unwrap();
            let writer = GatedWriter {
                entered: entered_tx,
                release: release_rx,
                fail: false,
                socket: Some(client),
            };
            let (peer, _, _) = kage_jsonrpc::connect(BufReader::new(server), writer);
            let attachment = test_attachment(peer.into());
            let bytes = Arc::new(AtomicUsize::new(0));
            let item = || {
                Buffered::new(json!({"text":"update"}), &bytes, Some(&attachment.bytes)).unwrap()
            };
            attachment
                .sender
                .as_ref()
                .unwrap()
                .send(WriteItem::Update(item()))
                .unwrap();
            entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            attachment
                .sender
                .as_ref()
                .unwrap()
                .send(WriteItem::Update(item()))
                .unwrap();
            let (ack, barrier) = mpsc::channel();
            attachment
                .sender
                .as_ref()
                .unwrap()
                .send(WriteItem::Barrier(ack))
                .unwrap();
            let mut state = State {
                upstream: Some("up".into()),
                early: Vec::new(),
                bytes: Arc::clone(&bytes),
                attachments: HashMap::from([(1, attachment)]),
                active: None,
                closed: false,
                cancels: None,
                writers: Arc::new(AtomicUsize::new(0)),
                asks: Vec::new(),
                recorder: None,
                history: std::collections::VecDeque::new(),
                history_bytes: 0,
                history_cut: false,
                run: kage_session::entry::EntryId::new().to_string(),
                turn: 0,
                turn_open: false,
                discard_replay: false,
            };
            state.close();
            assert!(state.attachments.contains_key(&1));
            release_tx.send(()).unwrap();
            state.attachments[&1]
                .worker_done
                .as_ref()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            assert!(barrier.recv_timeout(Duration::from_secs(3)).is_err());
            assert!(entered_rx.try_recv().is_err());
            assert_eq!(bytes.load(Ordering::Acquire), 0);
        }

        #[test]
        fn shutdown_stops_idle_worker_without_detaching_owner() {
            let session = scripted_session(ADMISSION_SCRIPT);
            session.terminate();
            let worker_done = {
                let mut state = lock(&session.state);
                assert!(state.closed);
                assert_eq!(state.attachments.len(), 1);
                state
                    .attachments
                    .get_mut(&1)
                    .unwrap()
                    .worker_done
                    .take()
                    .unwrap()
            };
            worker_done.recv_timeout(Duration::from_secs(3)).unwrap();
        }

        #[test]
        fn receive_exit_drops_full_queue_and_joins_reader() {
            struct QueuedReader {
                count: usize,
                queued: mpsc::Sender<()>,
            }
            impl Read for QueuedReader {
                fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                    if self.count == 9 {
                        return Ok(0);
                    }
                    let line =
                        b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{}}\n";
                    buffer[..line.len()].copy_from_slice(line);
                    self.count += 1;
                    if self.count == 9 {
                        self.queued.send(()).unwrap();
                    }
                    Ok(line.len())
                }
            }

            let (queued_tx, queued_rx) = mpsc::channel();
            let (read_end, write_end) = std::io::pipe().unwrap();
            let (connection, incoming, reader_thread) = UnixConnection::new_managed(
                BufReader::new(QueuedReader {
                    count: 0,
                    queued: queued_tx,
                }),
                write_end.into(),
            )
            .unwrap();
            queued_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let state = Arc::new(Mutex::new(State {
                upstream: None,
                early: Vec::new(),
                bytes: Arc::new(AtomicUsize::new(0)),
                attachments: HashMap::new(),
                active: None,
                closed: true,
                cancels: None,
                writers: Arc::new(AtomicUsize::new(0)),
                asks: Vec::new(),
                recorder: None,
                history: std::collections::VecDeque::new(),
                history_bytes: 0,
                history_cut: false,
                run: kage_session::entry::EntryId::new().to_string(),
                turn: 0,
                turn_open: false,
                discard_replay: false,
            }));
            let startup = Lifecycles::new(1).reserve().unwrap();
            startup.transport(
                connection.shutdown_handle(),
                Arc::new(AtomicBool::new(false)),
                reader_thread,
            );
            receive_updates(incoming, Arc::clone(&state), String::new(), startup.slot()).unwrap();
            let slot = startup.commit();
            assert!(slot.wait_finished(WAIT), "reader was not joined");
            drop(read_end);
        }

        #[test]
        fn receive_exit_joins_panicked_reader() {
            struct PanickingReader;
            impl Read for PanickingReader {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    panic!("reader failure");
                }
            }

            let (read_end, write_end) = std::io::pipe().unwrap();
            let (connection, incoming, reader_thread) =
                UnixConnection::new_managed(BufReader::new(PanickingReader), write_end.into())
                    .unwrap();
            let state = Arc::new(Mutex::new(State {
                upstream: None,
                early: Vec::new(),
                bytes: Arc::new(AtomicUsize::new(0)),
                attachments: HashMap::new(),
                active: None,
                closed: false,
                cancels: None,
                writers: Arc::new(AtomicUsize::new(0)),
                asks: Vec::new(),
                recorder: None,
                history: std::collections::VecDeque::new(),
                history_bytes: 0,
                history_cut: false,
                run: kage_session::entry::EntryId::new().to_string(),
                turn: 0,
                turn_open: false,
                discard_replay: false,
            }));
            let startup = Lifecycles::new(1).reserve().unwrap();
            startup.transport(
                connection.shutdown_handle(),
                Arc::new(AtomicBool::new(false)),
                reader_thread,
            );
            receive_updates(incoming, Arc::clone(&state), String::new(), startup.slot()).unwrap();
            let slot = startup.commit();
            assert!(slot.wait_finished(WAIT), "panicked reader was not joined");
            assert!(lock(&state).closed);
            drop(read_end);
        }

        #[test]
        fn reader_shutdown_does_not_require_writer_eof() {
            let (reader, writer) = std::io::pipe().unwrap();
            nix::fcntl::fcntl(
                &reader,
                nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
            )
            .unwrap();
            let stopped = Arc::new(AtomicBool::new(false));
            let mut reader = InterruptibleReader {
                reader,
                stopped: Arc::clone(&stopped),
            };
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let mut byte = [0];
                let _ = tx.send(reader.read(&mut byte));
            });
            stopped.store(true, Ordering::Release);
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(), 0);
            drop(writer);
        }

        #[test]
        fn shutdown_unblocks_incoming_and_transport_readers_with_inherited_stdout() {
            let script = r"
import json,sys,os,time
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new':
  if os.fork()==0:
   time.sleep(30)
   os._exit(0)
  result={'sessionId':'up'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            assert!(session.detach(1));
            assert!(
                session.slot.wait_finished(WAIT),
                "incoming reader remained blocked"
            );
        }

        #[test]
        fn receive_failure_interrupts_reader_and_prompt_with_escaped_stdout_holder() {
            struct ChildGuard {
                release: std::path::PathBuf,
                exited: std::path::PathBuf,
                finished: bool,
            }
            impl ChildGuard {
                fn finish(&mut self) -> bool {
                    if self.finished {
                        return true;
                    }
                    let _ = std::fs::write(&self.release, "release");
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while Instant::now() < deadline {
                        if std::fs::read(&self.exited).is_ok_and(|bytes| !bytes.is_empty()) {
                            self.finished = true;
                            return true;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    false
                }
            }
            impl Drop for ChildGuard {
                fn drop(&mut self) {
                    let _ = self.finish();
                }
            }

            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("ready");
            let release = dir.path().join("release");
            let exited = dir.path().join("exited");
            for path in [&ready, &release, &exited] {
                std::fs::write(path, "").unwrap();
            }
            let mut guard = ChildGuard {
                release: release.clone(),
                exited: exited.clone(),
                finished: false,
            };
            let script = format!(
                r"
import json,sys,os,time
ready_fd=os.open({ready:?},os.O_RDWR)
release_fd=os.open({release:?},os.O_RDONLY)
exited_fd=os.open({exited:?},os.O_RDWR)
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={{'protocolVersion':1,'agentCapabilities':{{}}}}
 elif method=='session/new': result={{'sessionId':'up'}}
 elif method=='session/prompt':
  if os.fork()==0:
   os.setsid()
   os.write(ready_fd,b'ready')
   end=time.monotonic()+15
   while time.monotonic()<end:
    os.lseek(release_fd,0,os.SEEK_SET)
    if os.read(release_fd,1): break
    time.sleep(0.005)
   os.close(1)
   os.write(exited_fd,b'exited')
   os._exit(0)
  while os.fstat(ready_fd).st_size==0: time.sleep(0.001)
  for _ in range(4):
   print(json.dumps({{'jsonrpc':'2.0','method':'session/update','params':{{'sessionId':'up','text':'x'*700000}}}}),flush=True)
  time.sleep(15)
  continue
 else: continue
 print(json.dumps({{'jsonrpc':'2.0','id':i,'result':result}}),flush=True)
",
                ready = ready.to_str().unwrap(),
                release = release.to_str().unwrap(),
                exited = exited.to_str().unwrap()
            );
            let session = scripted_session(&script);
            let (_client, server) = UnixStream::pair().unwrap();
            let (peer, _, _) =
                kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
            session.attach(2, peer.into()).unwrap();
            let active = Arc::clone(&session);
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(active.prompt_owned(1, request("overflow")));
            });
            assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().is_err());
            assert!(session.slot.wait_finished(WAIT));
            assert_eq!(std::fs::read(&ready).unwrap(), b"ready");
            assert!(std::fs::read(&exited).unwrap().is_empty());
            let state = lock(&session.state);
            assert!(state.closed);
            assert_eq!(state.attachments.len(), 2);
            drop(state);
            assert!(!session.detach(2));
            assert!(session.detach(1));
            assert!(
                guard.finish(),
                "escaped stdout holder did not acknowledge release"
            );
        }

        #[test]
        fn ignored_cancel_abandons_only_that_session() {
            let script = r"
import json,sys
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt': continue
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let session = scripted_session(script);
            let active = Arc::clone(&session);
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(active.prompt_owned(1, request("ignored")));
            });
            let until = Instant::now() + Duration::from_secs(3);
            while lock(&session.state)
                .active
                .as_ref()
                .is_none_or(|active| active.canceller.is_none())
            {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            session.cancel(1);
            assert!(
                rx.recv_timeout(TIMEOUT + Duration::from_secs(3))
                    .unwrap()
                    .is_err()
            );
            assert!(
                session
                    .prompt_owned(1, request("must not dispatch"))
                    .is_err()
            );
            assert!(session.slot.wait_finished(WAIT));
            assert!(session.detach(1));
        }

        #[test]
        fn upstream_eof_closes_session_even_if_process_stays_alive() {
            let script = r"
import json,sys,os,time
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  os.close(1)
  time.sleep(30)
  continue
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), script.into()],
                env: BTreeMap::default(),
            };
            let (a, b) = UnixStream::pair().unwrap();
            let (peer, _, _) = kage_jsonrpc::connect(BufReader::new(b.try_clone().unwrap()), b);
            let cwd = std::env::current_dir().unwrap();
            let (session, created) = External::start(
                &Lifecycles::new(1),
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd: cwd.to_str().unwrap(),
                    config: &config,
                    owner: 1,
                    peer: peer.clone().into(),
                    record: None,
                    restore: None,
                },
            )
            .unwrap();
            created.announce();
            let worker_done = {
                let mut state = lock(&session.state);
                state
                    .attachments
                    .get_mut(&1)
                    .unwrap()
                    .worker_done
                    .take()
                    .unwrap()
            };
            let active = Arc::clone(&session);
            let prompt = thread::spawn(move || {
                active.prompt_owned(
                    1,
                    PromptRequest {
                        session_id: "local".into(),
                        prompt: vec![ContentBlock::text("hi")],
                        delivery: None,
                    },
                )
            });
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while !lock(&session.state).closed {
                assert!(std::time::Instant::now() < until);
                thread::sleep(Duration::from_millis(5));
            }
            worker_done.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(lock(&session.state).attachments.contains_key(&1));
            assert!(session.attach(2, peer.into()).is_err());
            assert!(session.detach(1));
            assert!(session.slot.wait_finished(WAIT));
            assert!(prompt.join().unwrap().is_err());
            drop(a);
        }

        #[test]
        fn cancellation_of_queued_owner_does_not_cancel_another_owner_and_last_detach_reaps() {
            let script = r"
import json,sys,time
for line in sys.stdin:
 m=json.loads(line); method=m.get('method'); i=m.get('id')
 if method=='initialize': result={'protocolVersion':1,'agentCapabilities':{}}
 elif method=='session/new': result={'sessionId':'up'}
 elif method=='session/prompt':
  time.sleep(0.4)
  result={'stopReason':'end_turn'}
 else: continue
 print(json.dumps({'jsonrpc':'2.0','id':i,'result':result}),flush=True)
";
            let config = kage_core::config::AcpAgent {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), script.into()],
                env: BTreeMap::default(),
            };
            let (a, b) = UnixStream::pair().unwrap();
            let (peer, _incoming, _) =
                kage_jsonrpc::connect(BufReader::new(b.try_clone().unwrap()), b);
            let cwd = std::env::current_dir().unwrap();
            let (session, created) = External::start(
                &Lifecycles::new(1),
                Launch {
                    id: "local".into(),
                    name: "fake",
                    cwd: cwd.to_str().unwrap(),
                    config: &config,
                    owner: 1,
                    peer: peer.clone().into(),
                    record: None,
                    restore: None,
                },
            )
            .unwrap();
            session.attach(2, peer.into()).unwrap();
            created.announce();
            session.announce(2);
            let prompt = || PromptRequest {
                session_id: "local".into(),
                prompt: vec![ContentBlock::text("hi")],
                delivery: None,
            };
            let active = Arc::clone(&session);
            let first = thread::spawn(move || active.prompt_owned(1, prompt()).unwrap());
            let until = std::time::Instant::now() + Duration::from_secs(3);
            while lock(&session.state)
                .active
                .as_ref()
                .is_none_or(|active| active.owner != 1)
            {
                assert!(std::time::Instant::now() < until);
                thread::sleep(Duration::from_millis(2));
            }
            let ticket = session.register_prompt(2).unwrap();
            let queued = Arc::clone(&session);
            let second = thread::spawn(move || queued.prompt(ticket, prompt()).unwrap());
            session.cancel(2);
            assert_eq!(first.join().unwrap().stop_reason, StopReason::EndTurn);
            assert_eq!(second.join().unwrap().stop_reason, StopReason::Cancelled);
            assert_eq!(
                session.prompt_owned(2, prompt()).unwrap().stop_reason,
                StopReason::EndTurn
            );
            assert!(!session.detach(2));
            assert!(session.detach(1));
            assert!(session.slot.wait_finished(WAIT));
            drop(a);
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) use lifecycle::Lifecycles;
#[cfg(target_os = "linux")]
pub(super) use unix::{External, OpenTicket, PromptTicket};

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use kage_acp::acp::{PromptRequest, PromptResponse};
    use kage_acp::agent::AgentPeer;
    use kage_jsonrpc::RpcError;
    use std::sync::Arc;

    pub(crate) struct External;
    pub(crate) struct PromptTicket;

    /// No open can start without external session support.
    pub(crate) enum OpenTicket {}

    impl OpenTicket {
        pub(crate) fn session(&self) -> &Arc<External> {
            match *self {}
        }
        pub(crate) fn announce(self) {
            match self {}
        }
        pub(crate) fn fail(self) -> Option<bool> {
            match self {}
        }
        pub(crate) fn wait_history(&mut self) -> Result<(), RpcError> {
            match *self {}
        }
    }

    /// External sessions are unavailable, so no capacity is ever reserved.
    pub(crate) struct Lifecycles;

    impl Lifecycles {
        pub(crate) fn new(_: usize) -> Self {
            Self
        }
    }

    impl External {
        pub(crate) fn start(
            _: &Lifecycles,
            _: super::Launch<'_>,
        ) -> Result<(Arc<Self>, OpenTicket), RpcError> {
            Err(RpcError::internal("external ACP requires Linux"))
        }
        pub(crate) fn attach_open(
            self: &Arc<Self>,
            _: u64,
            _: AgentPeer,
        ) -> Result<OpenTicket, RpcError> {
            Err(RpcError::internal("external ACP requires Linux"))
        }
        pub(crate) fn attach_load(
            self: &Arc<Self>,
            _: u64,
            _: AgentPeer,
        ) -> Result<OpenTicket, RpcError> {
            Err(RpcError::internal("external ACP requires Linux"))
        }
        pub(crate) fn detach(&self, _: u64) -> bool {
            true
        }
        pub(crate) fn register_prompt(&self, _: u64) -> Result<PromptTicket, RpcError> {
            Err(RpcError::internal("external ACP requires Linux"))
        }
        pub(crate) fn prompt(
            &self,
            _: PromptTicket,
            _: PromptRequest,
        ) -> Result<PromptResponse, RpcError> {
            Err(RpcError::internal("external ACP requires Linux"))
        }
        pub(crate) fn cancel(&self, _: u64) {}
    }
}
#[cfg(not(target_os = "linux"))]
pub(super) use unsupported::{External, Lifecycles, OpenTicket, PromptTicket};
