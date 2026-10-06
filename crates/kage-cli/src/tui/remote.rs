//! The TUI attached to a session a running `kage serve` hosts.
//!
//! `kage resume` on a session file another process holds locked asks
//! every registered serve ([`crate::serve_registry`]) for the session
//! over its link socket ([`crate::rpc::link`]). When one hosts it, the
//! TUI runs without an engine of its own: commands go over the socket
//! and the envelopes coming back feed the App like a local engine's.
//! The plugin runtime loads no plugins and no `init.lua`, since Lua
//! runs in serve, so keymaps, options and themes still work but plugin
//! commands do not.

use super::*;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kage_core::SessionId;
use kage_core::protocol::{
    Command, CommandKind, Envelope, HostEvent, NoticeLevel, with_canonical_tool_names,
};
use kage_plugin::LogLevel;
use kage_tui::hostlog::LogPublisher;

use super::entry::{Frontend, startup_config};
use super::host::Link;
use crate::rpc::link::{Hello, LINK_VERSION, Reply, write_line};

/// How long a serve may take to accept or refuse an attach.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the App shows once the serve is gone.
const DETACHED: &str = "serve stopped; session detached";

/// A link a `kage serve` accepted for one session.
pub(crate) struct Attached {
    pid: u32,
    session: SessionId,
    path: PathBuf,
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

/// Attaches to the `kage serve` hosting the session at `path`, asking
/// every registered serve in turn. `None` when none hosts it.
pub(crate) fn attach(path: &Path) -> Option<Attached> {
    let session = crate::engine::session_id_of(path)?;
    let dir = crate::paths::runtime_dir().ok()?;
    crate::serve_registry::connect(&dir).find_map(|(_, stream)| handshake(stream, session, path))
}

fn handshake(stream: UnixStream, session: SessionId, path: &Path) -> Option<Attached> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok()?;
    let mut writer = stream.try_clone().ok()?;
    let hello = Hello::Attach {
        session,
        v: LINK_VERSION,
    };
    write_line(&mut writer, &hello).ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let Reply::Attached { pid } = serde_json::from_str(&line).ok()? else {
        return None;
    };
    reader.get_ref().set_read_timeout(None).ok()?;
    Some(Attached {
        pid,
        session,
        path: path.to_path_buf(),
        reader,
        writer,
    })
}

/// The serve end of an attached TUI.
pub(crate) struct Remote {
    /// The serve's process id.
    pub(crate) pid: u32,
    session: SessionId,
    writer: Mutex<Box<dyn Write + Send>>,
    /// Where the App's envelopes go, for notices raised on this side.
    events: mpsc::Sender<Envelope>,
    /// Set once the serve is gone.
    closed: Arc<AtomicBool>,
}

impl Remote {
    /// Starts reading envelopes from `reader` into `deliver` until the
    /// serve goes away, which the App hears of through `events`.
    fn start(
        pid: u32,
        session: SessionId,
        reader: impl BufRead + Send + 'static,
        writer: impl Write + Send + 'static,
        events: mpsc::Sender<Envelope>,
        mut deliver: impl FnMut(Envelope) + Send + 'static,
    ) -> Arc<Self> {
        let remote = Arc::new(Self {
            pid,
            session,
            writer: Mutex::new(Box::new(writer)),
            events,
            closed: Arc::default(),
        });
        let reading = Arc::clone(&remote);
        thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(envelope) = serde_json::from_str(&line) {
                    deliver(envelope);
                }
            }
            reading.closed.store(true, Ordering::SeqCst);
            reading.notice(NoticeLevel::Error, DETACHED, false);
        });
        remote
    }

    /// Sends `command` to the serve. Once the serve is gone every
    /// command is refused with a notice instead of vanishing.
    pub(crate) fn send(&self, command: &Command) {
        if !self.closed.load(Ordering::SeqCst) {
            let mut line = serde_json::to_vec(command).unwrap_or_default();
            line.push(b'\n');
            let mut writer = lock(&self.writer);
            if writer
                .write_all(&line)
                .and_then(|()| writer.flush())
                .is_ok()
            {
                return;
            }
            self.closed.store(true, Ordering::SeqCst);
        }
        self.notice(NoticeLevel::Error, DETACHED, false);
    }

    /// Shows `event` in the App as if the serve had published it.
    pub(crate) fn publish(&self, event: HostEvent) {
        let _ = self.events.send(Envelope {
            session: self.session,
            seq: 0,
            event: event.into(),
        });
    }

    fn notice(&self, level: NoticeLevel, text: &str, transient: bool) {
        self.publish(HostEvent::Notice {
            level,
            text: text.to_owned(),
            transient,
        });
    }
}

/// Runs the TUI on the session `attached` reaches, switched to `model`
/// when given. Returns the exit code once the user quits.
pub(crate) fn run(attached: Attached, model: Option<&str>) -> ExitCode {
    let registry = match crate::build_provider_registry() {
        Ok(registry) => Arc::new(registry),
        Err(e) => {
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    let buffer = shared_buffer();
    let toasts = shared_toasts();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    kage_tui::theme::detect_terminal_background();
    let Some((config, options)) = startup_config(&workdir, &buffer) else {
        return ExitCode::from(1);
    };
    let shown_model = model.map_or_else(|| crate::default_model(&registry), str::to_owned);
    let log_publisher: Arc<OnceLock<LogPublisher>> = Arc::default();
    let plugins = match setup_tui_runtime(
        None,
        None,
        config.plugins.clone(),
        config.keybindings.clone(),
        Arc::clone(&options),
        &workdir,
        &shown_model,
        "",
        buffer_host_log(buffer.clone(), toasts.clone(), Arc::clone(&log_publisher)),
    ) {
        Ok(rt) => Some(rt),
        Err(e) => {
            lock(&buffer).push_custom("kage:error", e, false);
            None
        }
    };
    if let Some(rt) = plugins.as_ref() {
        let ui = rt.slots().ui_state();
        let mut ui = lock(&ui);
        ui.session_id = attached.session.to_string();
        ui.cwd = workdir.display().to_string();
    }
    let session = attached.session;
    let mirror = Arc::new(Mutex::new(host::Mirror::new(Some(attached.path.clone()))));
    let (events_tx, events_rx) = mpsc::channel();
    let link = connect(attached, &config, &mirror, plugins.clone(), events_tx);
    if let Some(model) = model {
        link.send(Command::active(CommandKind::SetModel {
            model: model.to_owned(),
        }));
    }
    let log_link = link.clone();
    let _ = log_publisher.set(Box::new(move |level, message| {
        let level = match level {
            LogLevel::Error => NoticeLevel::Error,
            _ => NoticeLevel::Info,
        };
        log_link.publish(HostEvent::Notice {
            level,
            text: message.to_owned(),
            transient: false,
        });
    }));
    let (tx, rx) = mpsc::channel::<RunRequest>();
    let (dialog_tx, dialog_rx) = mpsc::channel::<PluginDialog>();
    let (plugin_refresh_tx, plugin_refresh_rx) = mpsc::channel::<PluginRefresh>();
    host::Host {
        link,
        registry: Arc::clone(&registry),
        plugins: plugins.clone(),
        plugins_dir: None,
        dialog_tx,
        plugin_refresh_tx,
        mirror: Arc::clone(&mirror),
        reported_shadows: std::collections::HashSet::new(),
    }
    .spawn(rx);
    Frontend {
        buffer,
        toasts,
        options,
        config,
        workdir,
        plugins,
        mirror,
        requests: tx,
        events: events_rx,
        dialogs: dialog_rx,
        refresh: plugin_refresh_rx,
        model_choices: available_model_items(&registry, &shown_model),
        model: shown_model,
        plugin_commands: Vec::new(),
        skills: Vec::new(),
        notices: Vec::new(),
        session,
    }
    .run(|| {})
}

/// Starts the link over `attached`: envelopes from the serve keep
/// `mirror` current and reach the App through `events` with tool names
/// made canonical, and the App hears that the TUI is attached.
fn connect(
    attached: Attached,
    config: &kage_core::config::Config,
    mirror: &Arc<Mutex<host::Mirror>>,
    plugins: Option<Arc<PluginRuntime>>,
    events: mpsc::Sender<Envelope>,
) -> Link {
    let aliases = kage_tools::builtin_registry()
        .with_renames(&config.tools.rename)
        .alias_map();
    let mut observe = host::mirror(Arc::clone(mirror), plugins);
    let app = events.clone();
    let remote = Remote::start(
        attached.pid,
        attached.session,
        attached.reader,
        attached.writer,
        events,
        move |envelope| {
            observe(&envelope);
            let _ = app.send(with_canonical_tool_names(envelope, &aliases));
        },
    );
    remote.notice(
        NoticeLevel::Info,
        &format!("attached to kage serve (pid {})", remote.pid),
        true,
    );
    Link::Remote(remote)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn notice_text(envelope: &Envelope) -> Option<&str> {
        match &envelope.event {
            kage_core::protocol::Event::Host(HostEvent::Notice { text, .. }) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn envelopes_reach_the_app_until_the_serve_goes_away() {
        let session = SessionId::new();
        let title = Envelope {
            session,
            seq: 3,
            event: HostEvent::TitleChanged { title: "t".into() }.into(),
        };
        let wire = format!("{}\n", serde_json::to_string(&title).unwrap());
        let (events, app) = mpsc::channel();
        let forward = events.clone();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let remote = Remote::start(
            7,
            session,
            Cursor::new(wire),
            SharedSink(Arc::clone(&sent)),
            events,
            move |envelope| {
                let _ = forward.send(envelope);
            },
        );
        let wait = Duration::from_secs(5);
        assert_eq!(app.recv_timeout(wait).unwrap(), title);
        let gone = app.recv_timeout(wait).unwrap();
        assert_eq!(notice_text(&gone), Some(DETACHED));
        remote.send(&Command::active(CommandKind::Prompt {
            content: vec![Content::Text { text: "hi".into() }],
            delivery: kage_core::protocol::Delivery::Queue,
        }));
        let refused = app.recv_timeout(wait).unwrap();
        assert_eq!(notice_text(&refused), Some(DETACHED));
        remote.send(&Command::active(CommandKind::Cancel));
        let refused_too = app.recv_timeout(wait).unwrap();
        assert_eq!(notice_text(&refused_too), Some(DETACHED));
        assert!(lock(&sent).is_empty(), "nothing goes out once detached");
    }

    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
