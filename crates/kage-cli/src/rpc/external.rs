//! Host-owned external ACP sessions.

#[cfg(unix)]
mod unix {

    use std::collections::HashMap;
    use std::io::{self, BufReader, Read};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use kage_acp::acp::{
        ClientCapabilities, ContentBlock, InitializeRequest, NewSessionRequest, PROTOCOL_VERSION,
        PromptCapabilities, PromptDelivery, PromptRequest, PromptResponse,
    };
    use kage_acp::agent::AgentPeer;
    use kage_acp::connection::{Message, UnixConnection, UnixPromptCanceller, UnixShutdown};
    use kage_core::{CancelFlag, sync::lock};
    use kage_jsonrpc::{ResponseBarrier, RpcError};
    use serde_json::{Value, json};

    const TIMEOUT: Duration = Duration::from_secs(10);
    const SESSION_BUDGET: usize = 8 * 1024 * 1024;
    const ATTACHMENT_BUDGET: usize = 2 * 1024 * 1024;
    const MAX_ATTACHMENTS: usize = 32;
    // Includes the active prompt and all registered tickets, until each is dropped.
    const MAX_PROMPTS: usize = 32;

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
        #[cfg(test)]
        done: Option<mpsc::Sender<()>>,
    }

    #[cfg(test)]
    impl<R> Drop for InterruptibleReader<R> {
        fn drop(&mut self) {
            if let Some(done) = self.done.take() {
                let _ = done.send(());
            }
        }
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
        pending: Vec<Buffered>,
        bytes: Arc<AtomicUsize>,
        detached: Arc<AtomicBool>,
    }

    enum WriteItem {
        Update(Buffered),
        Barrier(mpsc::Sender<()>),
    }

    struct State {
        upstream: Option<String>,
        early: Vec<Buffered>,
        bytes: Arc<AtomicUsize>,
        attachments: HashMap<u64, Attachment>,
        active: Option<Active>,
        closed: bool,
        #[cfg(test)]
        reader_done: bool,
    }

    impl State {
        fn close(&mut self) {
            self.closed = true;
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
        }
    }

    struct Active {
        owner: u64,
        identity: Arc<()>,
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

    fn send_cancel(action: CancelAction) {
        thread::spawn(move || run_cancel(&action));
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

    /// A single upstream process and session, shared by host attachments.
    pub(crate) struct External {
        connection: UnixConnection,
        reader_stopped: Arc<AtomicBool>,
        #[cfg(test)]
        reader_done: Mutex<mpsc::Receiver<()>>,
        prompt_gate: Mutex<()>,
        admitted: Arc<AtomicUsize>,
        child: Arc<Mutex<Option<Child>>>,
        capabilities: PromptCapabilities,
        state: Arc<Mutex<State>>,
    }

    impl External {
        pub(crate) fn start(
            id: String,
            name: &str,
            cwd: &str,
            config: &kage_core::config::AcpAgent,
            owner: u64,
            peer: AgentPeer,
        ) -> Result<Arc<Self>, RpcError> {
            let mut command = Command::new(&config.command);
            command.process_group(0);
            let mut child = command
                .args(&config.args)
                .envs(&config.env)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| {
                    RpcError::internal(format!("cannot start external agent {name:?}: {e}"))
                })?;
            let stdout = child.stdout.take().expect("piped stdout");
            let stdin = child.stdin.take().expect("piped stdin");
            if let Err(error) = nix::fcntl::fcntl(
                &stdout,
                nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
            ) {
                kill_group(&mut child);
                return Err(RpcError::internal(format!(
                    "cannot configure external reader: {error}"
                )));
            }
            let reader_stopped = Arc::new(AtomicBool::new(false));
            #[cfg(test)]
            let (reader_done_tx, reader_done) = mpsc::channel();
            let reader = InterruptibleReader {
                reader: stdout,
                stopped: Arc::clone(&reader_stopped),
                #[cfg(test)]
                done: Some(reader_done_tx),
            };
            let (mut connection, incoming) =
                match UnixConnection::new(BufReader::new(reader), stdin.into()) {
                    Ok(connection) => connection,
                    Err(error) => {
                        kill_group(&mut child);
                        return Err(RpcError::internal(format!(
                            "cannot connect external agent: {error}"
                        )));
                    }
                };
            let child = Arc::new(Mutex::new(Some(child)));
            let state = Arc::new(Mutex::new(State {
                upstream: None,
                early: Vec::new(),
                bytes: Arc::new(AtomicUsize::new(0)),
                attachments: HashMap::from([(owner, attachment(peer))]),
                active: None,
                closed: false,
                #[cfg(test)]
                reader_done: false,
            }));
            let sink = Arc::clone(&state);
            let session_id = id.clone();
            let external_id = id;
            let process = Arc::clone(&child);
            receive_updates(
                incoming,
                sink,
                external_id,
                process,
                Arc::clone(&reader_stopped),
                connection.shutdown_handle(),
            );
            let result = initialize(&mut connection, &state, cwd, &session_id);
            let capabilities = match result {
                Ok(value) => value,
                Err(error) => {
                    lock(&state).close();
                    reader_stopped.store(true, Ordering::Release);
                    connection.shutdown();
                    cleanup(&child);
                    return Err(error);
                }
            };
            Ok(Arc::new(Self {
                connection,
                reader_stopped,
                #[cfg(test)]
                reader_done: Mutex::new(reader_done),
                prompt_gate: Mutex::new(()),
                admitted: Arc::new(AtomicUsize::new(0)),
                child,
                capabilities,
                state,
            }))
        }

        pub(crate) fn attach(&self, owner: u64, peer: AgentPeer) -> Result<(), RpcError> {
            let mut state = lock(&self.state);
            if state.closed {
                return Err(RpcError::internal("external session is closed"));
            }
            if !state.attachments.contains_key(&owner) && state.attachments.len() >= MAX_ATTACHMENTS
            {
                return Err(RpcError::internal("too many external attachments"));
            }
            state
                .attachments
                .entry(owner)
                .or_insert_with(|| attachment(peer));
            Ok(())
        }

        pub(crate) fn announce(&self, owner: u64) {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            let mut failed = false;
            if let Some(attachment) = state.attachments.get_mut(&owner) {
                attachment.announced = true;
                for update in attachment.pending.drain(..) {
                    if attachment
                        .sender
                        .as_ref()
                        .expect("open attachment")
                        .try_send(WriteItem::Update(update))
                        .is_err()
                    {
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                state.close();
            }
            drop(state);
            if failed {
                self.shutdown();
            }
        }

        pub(crate) fn detach(&self, owner: u64) -> bool {
            let (last, cancel) = self.detach_mark(owner);
            self.finish_detach(last, cancel);
            last
        }

        pub(crate) fn detach_mark(&self, owner: u64) -> (bool, Option<CancelAction>) {
            let mut state = lock(&self.state);
            let Some(attachment) = state.attachments.remove(&owner) else {
                return (false, None);
            };
            attachment.detached.store(true, Ordering::Release);
            let last = state.attachments.is_empty();
            if last {
                state.close();
            }
            let cancel = if last {
                None
            } else {
                state
                    .active
                    .as_mut()
                    .filter(|active| {
                        active.owner == owner && Arc::ptr_eq(&active.identity, &attachment.identity)
                    })
                    .and_then(|active| active.cancel(&self.state))
            };
            (last, cancel)
        }

        pub(crate) fn finish_detach(&self, last: bool, cancel: Option<CancelAction>) {
            if last {
                self.shutdown();
            } else if let Some(canceller) = cancel {
                send_cancel(canceller);
            }
        }

        fn shutdown(&self) {
            lock(&self.state).close();
            self.reader_stopped.store(true, Ordering::Release);
            self.connection.shutdown();
            cleanup(&self.child);
        }

        pub(crate) fn register_prompt(&self, owner: u64) -> Result<PromptTicket, RpcError> {
            let state = lock(&self.state);
            let Some(attachment) = state.attachments.get(&owner).filter(|_| !state.closed) else {
                return Err(RpcError::new(-32602, "external session is not attached"));
            };
            if !reserve(&self.admitted, 1, MAX_PROMPTS) {
                return Err(RpcError::internal("too many external prompts"));
            }
            Ok(PromptTicket {
                owner,
                identity: Arc::clone(&attachment.identity),
                epoch: attachment.epoch,
                admitted: Arc::clone(&self.admitted),
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
                state.active = Some(Active {
                    owner,
                    identity: Arc::clone(&ticket.identity),
                    cancelled: false,
                    canceller: None,
                    abort: abort.clone(),
                    cancellation: Cancellation::None,
                });
            }
            req.delivery = None;
            let deadline = Instant::now() + TIMEOUT;
            let prompt = match self.connection.dispatch_prompt(&req, Some(deadline), None) {
                Ok(prompt) => prompt,
                Err(error) => {
                    let mut state = lock(&self.state);
                    state.active = None;
                    state.close();
                    drop(state);
                    self.shutdown();
                    return Err(error);
                }
            };
            let action = {
                let mut state = lock(&self.state);
                let active = state.active.as_mut().expect("active prompt");
                active.canceller = Some(prompt.canceller());
                if active.cancelled {
                    active.cancel_action(&self.state)
                } else {
                    None
                }
            };
            if let Some(action) = action {
                send_cancel(action);
            }
            let mut result = prompt.wait(None, TIMEOUT, Some(&abort));
            let active = lock(&self.state).active.take().expect("active prompt");
            let cancelled = active.cancelled;
            if self.cancellation_write_failed(active.cancellation) {
                lock(&self.state).close();
                self.shutdown();
                return Err(RpcError::internal(
                    "external cancellation write did not finish",
                ));
            }
            if let Ok(response) = &mut result
                && cancelled
            {
                response.stop_reason = kage_acp::acp::StopReason::Cancelled;
            }
            if result.is_err() {
                lock(&self.state).close();
                self.shutdown();
                return result;
            }
            self.wait_for_writes(&ticket)?;
            result
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
                    state.close();
                    drop(state);
                    self.shutdown();
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
                state.close();
                drop(state);
                self.shutdown();
            }
        }

        pub(crate) fn cancel(&self, owner: u64) {
            let canceller = {
                let mut state = lock(&self.state);
                let Some(attachment) = state.attachments.get_mut(&owner) else {
                    return;
                };
                attachment.epoch = attachment.epoch.wrapping_add(1);
                let identity = Arc::clone(&attachment.identity);
                state
                    .active
                    .as_mut()
                    .filter(|active| {
                        active.owner == owner && Arc::ptr_eq(&active.identity, &identity)
                    })
                    .and_then(|active| active.cancel(&self.state))
            };
            if let Some(canceller) = canceller {
                send_cancel(canceller);
            }
        }
    }

    fn receive_updates(
        incoming: kage_acp::connection::Incoming,
        sink: Arc<Mutex<State>>,
        external_id: String,
        process: Arc<Mutex<Option<Child>>>,
        reader_stopped: Arc<AtomicBool>,
        shutdown: UnixShutdown,
    ) {
        thread::spawn(move || {
            while let Ok(message) = incoming.recv() {
                match message {
                    Message::Notification { method, params } if method == "session/update" => {
                        let mut state = lock(&sink);
                        if state.closed {
                            break;
                        }
                        if state.upstream.is_none() {
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
                    Message::Request(mut request) => {
                        let response = if request.method == "session/request_permission" {
                            Ok(json!({"outcome": {"outcome": "cancelled"}}))
                        } else {
                            Err(RpcError::method_not_found(&request.method))
                        };
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
            let abort = {
                let mut state = lock(&sink);
                state.close();
                #[cfg(test)]
                {
                    state.reader_done = true;
                }
                state.active.as_ref().map(|active| active.abort.clone())
            };
            reader_stopped.store(true, Ordering::Release);
            if let Some(abort) = abort {
                abort.cancel();
            }
            shutdown.shutdown();
            cleanup(&process);
        });
    }

    fn kill_group(child: &mut Child) {
        let pgid = nix::unistd::Pid::from_raw(child.id().cast_signed());
        if nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL).is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }

    fn cleanup(child: &Mutex<Option<Child>>) {
        if let Some(mut child) = lock(child).take() {
            kill_group(&mut child);
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
        id: &str,
    ) -> Result<PromptCapabilities, RpcError> {
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
        let mut state = lock(state);
        state.upstream = Some(response.session_id);
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
        Ok(init.agent_capabilities.prompt_capabilities)
    }

    // A generic Peer cannot interrupt a write already in progress. Detaching
    // and terminal closure suppress queued updates but cannot guarantee that an
    // update already selected for writing will not arrive after the stop.
    // External sessions must remain disabled until the downstream transport
    // provides interruptible, ordered writes.
    fn attachment(peer: AgentPeer) -> Attachment {
        let (sender, receiver) = mpsc::sync_channel(1024);
        let detached = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&detached);
        #[cfg(test)]
        let (worker_done_tx, worker_done) = mpsc::channel();
        thread::spawn(move || {
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
                    WriteItem::Barrier(done) => {
                        let _ = done.send(());
                    }
                }
            }
            #[cfg(test)]
            let _ = worker_done_tx.send(());
        });
        Attachment {
            sender: Some(sender),
            #[cfg(test)]
            worker_done: Some(worker_done),
            identity: Arc::new(()),
            epoch: 0,
            announced: false,
            pending: Vec::new(),
            bytes: Arc::new(AtomicUsize::new(0)),
            detached,
        }
    }

    fn forward(state: &mut State, id: &str, mut params: Value) {
        if state.closed {
            return;
        }
        if params.get("sessionId").and_then(Value::as_str) != state.upstream.as_deref() {
            return;
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
            lock(&self.state).close();
            self.reader_stopped.store(true, Ordering::Release);
            self.connection.shutdown();
            cleanup(&self.child);
        }
    }

    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        use kage_acp::acp::StopReason;
        use std::collections::BTreeMap;
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixStream;

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
            let session = External::start(
                "local".into(),
                "fake",
                cwd.to_str().unwrap(),
                &config,
                1,
                peer.into(),
            )
            .unwrap();
            session.announce(1);
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
            let session =
                External::start("local".into(), "fake", cwd, &config, 1, peer.into()).unwrap();
            session.announce(1);
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
            let session = External::start(
                "local".into(),
                "fake",
                cwd.to_str().unwrap(),
                &config,
                1,
                peer.into(),
            )
            .unwrap();
            session.announce(1);
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
            session.shutdown();
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
            let (last, action) = session.detach_mark(1);
            assert!(!last);
            assert!(action.is_some());
            session.finish_detach(last, action);
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
                attachments: HashMap::from([(1, attachment(peer.into()))]),
                active: None,
                closed: false,
                reader_done: false,
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
            let attachment = attachment(peer.into());
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
            let attachment = attachment(peer.into());
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
                reader_done: false,
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
            session.shutdown();
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
                done: None,
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
            let until = Instant::now() + Duration::from_secs(3);
            while !lock(&session.state).reader_done {
                assert!(Instant::now() < until, "incoming reader remained blocked");
                thread::sleep(Duration::from_millis(2));
            }
            lock(&session.reader_done)
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
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
            lock(&session.reader_done)
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
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
            assert!(lock(&session.child).is_none());
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
            let session = External::start(
                "local".into(),
                "fake",
                cwd.to_str().unwrap(),
                &config,
                1,
                peer.clone().into(),
            )
            .unwrap();
            session.announce(1);
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
            assert!(lock(&session.child).is_none());
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
            let session = External::start(
                "local".into(),
                "fake",
                cwd.to_str().unwrap(),
                &config,
                1,
                peer.clone().into(),
            )
            .unwrap();
            session.attach(2, peer.into()).unwrap();
            session.announce(1);
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
            assert!(lock(&session.child).is_none());
            drop(a);
        }
    }
}

#[cfg(unix)]
pub(super) use unix::{External, PromptTicket};

#[cfg(not(unix))]
mod unsupported {
    use kage_acp::acp::{PromptRequest, PromptResponse};
    use kage_acp::agent::AgentPeer;
    use kage_jsonrpc::RpcError;
    use std::sync::Arc;

    pub(crate) struct External;
    pub(crate) struct CancelAction;
    pub(crate) struct PromptTicket;
    impl External {
        pub(crate) fn start(
            _: String,
            _: &str,
            _: &str,
            _: &kage_core::config::AcpAgent,
            _: u64,
            _: AgentPeer,
        ) -> Result<Arc<Self>, RpcError> {
            Err(RpcError::internal("external ACP requires Unix"))
        }
        pub(crate) fn attach(&self, _: u64, _: AgentPeer) -> Result<(), RpcError> {
            Err(RpcError::internal("external ACP requires Unix"))
        }
        pub(crate) fn announce(&self, _: u64) {}
        pub(crate) fn detach(&self, _: u64) -> bool {
            true
        }
        pub(crate) fn detach_mark(&self, _: u64) -> (bool, Option<CancelAction>) {
            (true, None)
        }
        pub(crate) fn finish_detach(&self, _: bool, _: Option<CancelAction>) {}
        pub(crate) fn register_prompt(&self, _: u64) -> Result<PromptTicket, RpcError> {
            Err(RpcError::internal("external ACP requires Unix"))
        }
        pub(crate) fn prompt(
            &self,
            _: PromptTicket,
            _: PromptRequest,
        ) -> Result<PromptResponse, RpcError> {
            Err(RpcError::internal("external ACP requires Unix"))
        }
        pub(crate) fn cancel(&self, _: u64) {}
    }
}
#[cfg(not(unix))]
pub(super) use unsupported::{External, PromptTicket};
