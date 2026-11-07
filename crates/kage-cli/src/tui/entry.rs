//! Interactive TUI entry point: `run_tui`.

use super::*;

use kage_core::ThinkingLevel;
use kage_core::options::{OptionStore, OptionValue};
use kage_core::protocol::{Envelope, NoticeLevel};

use kage_tui::TranscriptScope;

/// Drop into the interactive TUI, on the recorded session at `resume`
/// when given, the way the session picker resumes one. A session
/// another process holds is attached to through the `kage serve`
/// hosting it, when one does. The TUI speaks ACP: by default to the
/// user's shared host, started when it is not running, with
/// `--standalone` or where the shared host cannot be used to a private
/// host in this process, and with `KAGE_LINK=acp` to a `kage rpc`
/// child. Returns the appropriate process exit code once the user quits.
pub fn run_tui(
    model: Option<&str>,
    system: &str,
    resume: Option<&std::path::Path>,
    yolo: bool,
    standalone: bool,
) -> ExitCode {
    if std::env::var_os("KAGE_LINK").is_some_and(|link| link == "acp") {
        return super::acp::run(model, system, resume, yolo, super::acp::Upstream::Child);
    }
    #[cfg(unix)]
    if let Some(path) = resume
        && kage_session::is_locked(path)
        && let Some(attached) = super::remote::attach(path)
    {
        return super::remote::run(attached, model);
    }
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::trust::confirm_project_trust(&workdir);
    super::acp::run(
        model,
        system,
        resume,
        yolo,
        super::acp::Upstream::Decide { standalone },
    )
}

/// Load the layered config for `workdir` and seed the options from it
/// before any Lua runs, so `init.lua` overrides them. Option values
/// that do not apply are shown in `buffer` and keep their defaults.
/// `None` once a config error that must stop kage is printed.
pub(super) fn startup_config(
    workdir: &std::path::Path,
    buffer: &kage_tui::SharedBuffer,
) -> Option<(kage_core::config::Config, kage_plugin::SharedOptions)> {
    let config = match kage_core::config::Config::load_layered(workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kage: {e}");
            return None;
        }
    };
    // Structurally broken permission rules are a hard error: kage
    // would silently misapply them otherwise. Mirrors the providers
    // validation in `build_provider_registry`.
    if let Err(e) = config.permissions.validate() {
        eprintln!("kage: {e}");
        return None;
    }
    if let Err(e) = config.shell.validate() {
        eprintln!("kage: {e}");
        return None;
    }
    let (store, option_errors) = OptionStore::from_config(&config);
    for err in option_errors {
        let mut buf = lock(buffer);
        buf.push_custom("kage:error", format!("config: {err}"), false);
    }
    Some((config, Arc::new(Mutex::new(store))))
}

/// What the App runs with, whichever engine it drives.
pub(super) struct Frontend {
    pub(super) buffer: kage_tui::SharedBuffer,
    pub(super) toasts: kage_tui::SharedToasts,
    pub(super) options: kage_plugin::SharedOptions,
    pub(super) config: kage_core::config::Config,
    pub(super) workdir: PathBuf,
    pub(super) plugins: Option<Arc<PluginRuntime>>,
    pub(super) mirror: Arc<Mutex<Mirror>>,
    pub(super) requests: mpsc::Sender<RunRequest>,
    pub(super) events: mpsc::Receiver<Envelope>,
    pub(super) dialogs: mpsc::Receiver<PluginDialog>,
    pub(super) refresh: mpsc::Receiver<PluginRefresh>,
    /// The feed an ACP connection's pump fills. `None` on the link
    /// protocol, where the App reads the serve's envelopes.
    pub(super) acp_feed: Option<Arc<dyn kage_tui::AcpFeed>>,
    /// The model the status line shows until the engine reports one.
    pub(super) model: String,
    pub(super) model_choices: Vec<PickItem>,
    pub(super) plugin_commands: Vec<kage_tui::command::PluginCommand>,
    pub(super) skills: Vec<kage_core::Skill>,
    /// Lines the start card shows.
    pub(super) notices: Vec<(NoticeLevel, String)>,
    /// The session the TUI starts on.
    pub(super) session: kage_core::SessionId,
}

impl Frontend {
    /// Run the App until the user quits, call `stop` once the screen is
    /// restored, and print the exit summary.
    pub(super) fn run(self, stop: impl FnOnce()) -> ExitCode {
        let mut tui = match Tui::enter() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("kage: failed to enter raw mode: {e}");
                return ExitCode::from(1);
            }
        };
        let buffer = self.buffer.clone();
        let options = Arc::clone(&self.options);
        let mirror = Arc::clone(&self.mirror);
        let mut app = self.into_app();
        let result = app.run(&mut tui);
        let width = tui.terminal().size().map_or(80, |size| size.width);
        drop(tui);
        drop(app);
        stop();
        match result {
            Ok(_) => {
                print_exit_summary(&buffer, width, &options, lock(&mirror).path());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("kage: tui error: {e}");
                ExitCode::from(1)
            }
        }
    }

    fn into_app(self) -> App {
        let mut app = App::new(self.buffer.clone(), self.requests);
        app.set_model_choices(self.model_choices);
        app.set_history(crate::history::load());
        app.set_status_model(Arc::new(Mutex::new(self.model)));
        app.set_engine_events(self.events);
        app.set_plugin_commands(self.plugin_commands);
        app.set_skills(self.skills);
        // `:login` runs the interactive credential flow in the real
        // terminal (the App suspends itself around the call) and then
        // refreshes providers through the worker.
        {
            let config_for_login = self.config.clone();
            app.set_login_runner(std::sync::Arc::new(move |provider| {
                let ok = crate::auth::run_login(provider, &config_for_login) == ExitCode::SUCCESS;
                if ok && let Some(provider) = provider {
                    let _ = crate::state::clear_auth_failure(provider);
                }
                ok
            }));
        }
        // `/mcp login` runs the flow of `kage mcp login` the same way, and
        // the App restarts the server once it succeeds.
        {
            let mut servers = self.config.mcp.servers.clone();
            if let Some(rt) = self.plugins.as_ref() {
                servers.extend(rt.registered_mcp_servers());
            }
            app.set_mcp_login_runner(std::sync::Arc::new(move |server| {
                crate::mcp_auth::tui_login(server, &servers)
            }));
        }
        app.set_workdir(self.workdir.clone());
        if let Ok(dir) = crate::themes_dir() {
            app.set_themes_dir(dir);
        }
        let setter = self.plugins.clone().map(|rt| {
            Box::new(move |name: &str, value: OptionValue| {
                rt.set_option(name, value).map_err(|e| e.to_string())
            }) as kage_tui::OptionSetter
        });
        app.set_options(Arc::clone(&self.options), setter);
        if let Some(rt) = self.plugins.as_ref() {
            app.attach_plugins(rt);
        }
        app.set_plugin_dialog(self.dialogs);
        app.set_plugin_refresh(self.refresh);
        if let Some(feed) = self.acp_feed {
            app.set_acp_feed(feed);
        }
        app.set_toasts(self.toasts.clone());
        app.set_session_usage(shared_session_usage());
        app.set_status_session_id(self.session.to_string().chars().take(8).collect());
        let permissions = if self.config.permissions.is_default() {
            "built-in tools run without asking"
        } else {
            "configured rules"
        };
        let start = kage_tui::StartInfo {
            sessions: Vec::new(),
            notices: self.notices,
            permissions: permissions.to_owned(),
        };
        wire_sessions(&mut app, &self.workdir, &self.mirror);
        app.set_start_info(start);
        app
    }
}

/// Give the App the recorded sessions of `workdir` for its start card,
/// picker and tree, and a loader for agent transcripts beside the
/// session file `mirror` points at.
fn wire_sessions(app: &mut App, workdir: &std::path::Path, mirror: &Arc<Mutex<Mirror>>) {
    if let Ok(dir) = crate::sessions_dir() {
        let sessions_cache = Arc::new(Mutex::new(kage_session::SessionCache::default()));
        // The first paint must not wait on the scan: the start card's
        // recent sessions arrive on this channel once the thread is
        // done, and the warmed cache makes the first Ctrl+S quick.
        let (sessions_tx, sessions_rx) = mpsc::channel();
        let scan_dir = dir.clone();
        let scan_workdir = workdir.to_path_buf();
        let scan_cache = Arc::clone(&sessions_cache);
        thread::spawn(move || {
            let _ = sessions_tx.send(list_session_choices(
                &scan_dir,
                &scan_workdir,
                false,
                &mut lock(&scan_cache),
            ));
        });
        let tree_dir = dir.clone();
        let tree_mirror = Arc::clone(mirror);
        let lister_workdir = workdir.to_path_buf();
        app.set_session_lister(Box::new(move |all| {
            list_session_choices(&dir, &lister_workdir, all, &mut lock(&sessions_cache))
        }));
        app.set_session_tree_source(Box::new(move || {
            list_session_nodes(&tree_dir, lock(&tree_mirror).path())
        }));
        app.set_start_sessions(sessions_rx);
    }
    let loader_mirror = Arc::clone(mirror);
    app.set_agent_loader(Box::new(move |session| {
        let dir = lock(&loader_mirror).path()?.parent()?.to_path_buf();
        let replay = kage_session::replay(&dir.join(format!("{session}.jsonl"))).ok()?;
        Some(kage_tui::StoredAgent {
            messages: replay.history.into_iter().map(Arc::new).collect(),
            compacted_agents: replay.compacted_agents.into_iter().map(Arc::new).collect(),
        })
    }));
}

/// Print what stays in the terminal once the alt screen is gone: the
/// transcript `transcript_on_exit` asks for at `width`, then the
/// session file when one was recorded. Nothing runs any more, so live
/// text is finished and unfinished tool calls and agent cards read as
/// interrupted.
fn print_exit_summary(
    buffer: &kage_tui::SharedBuffer,
    width: u16,
    options: &kage_plugin::SharedOptions,
    session: Option<&std::path::Path>,
) {
    let scope = lock(options)
        .get("transcript_on_exit")
        .and_then(OptionValue::as_str)
        .and_then(TranscriptScope::parse)
        .unwrap_or(TranscriptScope::Full);
    let transcript = {
        let mut buffer = lock(buffer);
        buffer.finish_streaming();
        buffer.interrupt_running_tools();
        kage_tui::transcript::render(&mut buffer, width, scope)
    };
    if !transcript.is_empty() {
        println!("{transcript}\n");
    }
    if let Some(path) = session.filter(|path| path.exists()) {
        println!("session saved to {}", path.display());
        if let Some(id) = crate::engine::session_id_of(path) {
            println!("resume it with `kage resume {id}`");
        }
    }
}

/// Read the options that apply when a session starts: the loop config
/// and the thinking level. Called after the runtime loaded `init.lua`,
/// so values set there reach the first session.
pub(crate) fn startup_options(
    options: &kage_plugin::SharedOptions,
) -> (LoopConfig, Option<ThinkingLevel>) {
    let store = lock(options);
    let mut loop_cfg = LoopConfig::default();
    if let Some(threshold) = store
        .get("compaction_threshold")
        .and_then(OptionValue::as_float)
    {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the threshold is a fraction that f32 holds"
        )]
        {
            loop_cfg.compaction_threshold = threshold as f32;
        }
    }
    if let Some(retries) = store
        .get("max_provider_retries")
        .and_then(OptionValue::as_int)
    {
        loop_cfg.max_provider_retries = u32::try_from(retries).unwrap_or(0);
    }
    let thinking = store
        .get("thinking_level")
        .and_then(OptionValue::as_str)
        .and_then(ThinkingLevel::parse);
    (loop_cfg, thinking)
}
