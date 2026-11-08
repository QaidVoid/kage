//! Host routing for creating and restoring external ACP sessions.
//!
//! Agent launch settings come only from trusted configuration, never from a
//! session file, which holds just the agent's name and upstream identity.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use kage_acp::acp::{McpServer, NewSessionResponse};
use kage_acp::agent::OpenReceipt;
use kage_core::config::{AcpAgent, Config};
use kage_core::sync::lock;
use kage_jsonrpc::RpcError;
use kage_session::entry::{SessionBackend, SessionEntry};

use super::{CliAcpAgent, ExternalOpen, OpenFence, external};

impl CliAcpAgent {
    /// Create a session on the configured external agent `name`.
    pub(super) fn new_external(
        &self,
        name: &str,
        cwd: &str,
        servers: &[McpServer],
    ) -> Result<(NewSessionResponse, OpenReceipt), RpcError> {
        self.external_allowed(servers)?;
        if !Path::new(cwd).is_absolute() {
            return Err(RpcError::new(
                -32602,
                "an external session needs an absolute working directory",
            ));
        }
        let config = trusted_agent(name, cwd)?;
        let (path, mut header) = crate::plan_session_in(&self.host.sessions, "external", "");
        header.cwd = PathBuf::from(cwd);
        let id = header.session.to_string();
        let started = external::External::start(
            &self.host.lifecycles,
            external::Launch {
                id: id.clone(),
                name,
                cwd,
                config: &config,
                owner: self.connection,
                peer: self.peer.clone(),
                record: Some(external::Recording::Create {
                    path,
                    header: Box::new(header),
                }),
                restore: None,
            },
        )?;
        let receipt = self.register_external(&id, started, None)?;
        Ok((
            NewSessionResponse {
                session_id: id,
                config_options: Vec::new(),
            },
            receipt,
        ))
    }

    /// Restore the recorded external session `id`, which is not live, from
    /// its upstream agent. Returns `None` for a native or unknown session.
    pub(super) fn restore_external(
        &self,
        id: &str,
        servers: &[McpServer],
        fence: Option<&OpenFence>,
        history: bool,
    ) -> Result<Option<OpenReceipt>, RpcError> {
        let Some(path) = self.exact_session_path(id)? else {
            return Ok(None);
        };
        let Some((cwd, backend)) = external_identity(&path)? else {
            return Ok(None);
        };
        self.external_allowed(servers)?;
        let cwd = cwd
            .to_str()
            .ok_or_else(|| RpcError::internal("external session directory is not UTF-8"))?
            .to_owned();
        let config = trusted_agent(&backend.agent_name, &cwd)?;
        let Some(_restoring) = Restoring::begin(&self.host, id)? else {
            return self.attach_external(id, servers, fence, history);
        };
        let writer = kage_session::SessionWriter::open(&path).map_err(|error| match error {
            kage_session::SessionError::Locked { .. } => {
                RpcError::internal("the external session is still in use; try again")
            }
            other => RpcError::internal(format!("cannot open external session file: {other}")),
        })?;
        let started = external::External::start(
            &self.host.lifecycles,
            external::Launch {
                id: id.to_owned(),
                name: &backend.agent_name,
                cwd: &cwd,
                config: &config,
                owner: self.connection,
                peer: self.peer.clone(),
                record: Some(external::Recording::Append {
                    writer: Box::new(writer),
                }),
                restore: Some(external::Restore {
                    upstream: backend.upstream_session_id,
                    history,
                }),
            },
        )?;
        self.register_external(id, started, fence).map(Some)
    }

    fn external_allowed(&self, servers: &[McpServer]) -> Result<(), RpcError> {
        if !servers.is_empty() {
            return Err(RpcError::new(
                -32602,
                "external sessions do not support downstream MCP servers",
            ));
        }
        #[cfg(unix)]
        let native = self.peer.splits_requests();
        #[cfg(not(unix))]
        let native = false;
        if !native {
            return Err(RpcError::new(
                -32602,
                "external sessions need the native transport",
            ));
        }
        Ok(())
    }

    fn exact_session_path(&self, id: &str) -> Result<Option<PathBuf>, RpcError> {
        let path = kage_session::find_by_prefix(&self.host.sessions, id)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(path.filter(|path| path.file_stem().and_then(|stem| stem.to_str()) == Some(id)))
    }

    // Publish a started session under `id` and claim it for this
    // connection. A disconnect or a close fenced in the meantime fails the
    // creator's open, which closes the new session.
    fn register_external(
        &self,
        id: &str,
        (session, created): (Arc<external::External>, external::OpenTicket),
        fence: Option<&OpenFence>,
    ) -> Result<OpenReceipt, RpcError> {
        let mut open = ExternalOpen {
            host: Arc::clone(&self.host),
            ids: Arc::clone(&self.ids),
            id: id.to_owned(),
            ticket: None,
        };
        let mut sessions = lock(&self.host.external);
        sessions.retain(|_, live| !live.is_closed());
        let mut ids = lock(&self.ids);
        ids.external.retain(|id| sessions.contains_key(id));
        if session.is_closed() {
            return Err(RpcError::internal("external session ended while opening"));
        }
        if self.disconnecting.load(Ordering::SeqCst) {
            return Err(RpcError::internal("client is disconnecting"));
        }
        if fence.is_some_and(|fence| fence.closed.load(Ordering::SeqCst)) {
            return Err(RpcError::new(
                -32602,
                "the session was closed before it finished opening",
            ));
        }
        if sessions.contains_key(id) {
            return Err(RpcError::internal("external session is already live"));
        }
        sessions.insert(id.to_owned(), session);
        ids.external.insert(id.to_owned());
        open.ticket = Some(created);
        drop(ids);
        drop(sessions);
        if let Some(ticket) = open.ticket.as_mut() {
            ticket.wait_history()?;
        }
        Ok(Box::new(move || open.announce()))
    }
}

/// Marks a session as being restored, so two connections cannot start two
/// processes for one upstream session. The file lock taken next also keeps
/// out other hosts and a process of this one that is still closing.
struct Restoring<'a> {
    host: &'a super::host::Host,
    id: String,
}

impl<'a> Restoring<'a> {
    // `None` if the session became live meanwhile, so the caller attaches.
    fn begin(host: &'a super::host::Host, id: &str) -> Result<Option<Self>, RpcError> {
        let sessions = lock(&host.external);
        if sessions.contains_key(id) {
            return Ok(None);
        }
        if !lock(&host.restoring).insert(id.to_owned()) {
            return Err(RpcError::internal(
                "the external session is being restored; try again",
            ));
        }
        drop(sessions);
        Ok(Some(Self {
            host,
            id: id.to_owned(),
        }))
    }
}

impl Drop for Restoring<'_> {
    fn drop(&mut self) {
        lock(&self.host.restoring).remove(&self.id);
    }
}

// The launch settings of `name` from trusted configuration for `cwd`.
fn trusted_agent(name: &str, cwd: &str) -> Result<AcpAgent, RpcError> {
    let config = Config::load_layered(Path::new(cwd))
        .map_err(|e| RpcError::internal(format!("cannot load configuration for {cwd}: {e}")))?;
    config
        .acp
        .agents
        .get(name)
        .cloned()
        .ok_or_else(|| RpcError::new(-32602, format!("unknown external agent {name}")))
}

// The recorded working directory and backend identity of an external
// session file, or `None` for a native one.
fn external_identity(
    path: &Path,
) -> Result<Option<(PathBuf, kage_session::entry::ExternalAcpSession)>, RpcError> {
    let mut reader = kage_session::SessionReader::iter(path)
        .map_err(|_| RpcError::internal("cannot read session header"))?;
    match reader.next() {
        Some(Ok(SessionEntry::Header(header))) => match header.backend {
            Some(SessionBackend::ExternalAcp(backend)) => Ok(Some((header.cwd, backend))),
            None => Ok(None),
        },
        _ => Err(RpcError::internal("cannot read session header")),
    }
}
