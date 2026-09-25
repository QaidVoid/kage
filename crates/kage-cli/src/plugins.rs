//! Plugin integration for the CLI.
//!
//! [`setup_runtime`] builds a [`PluginRuntime`] for print mode and
//! `kage rpc`, loads every `*.lua` file in `plugins_dir`, and returns
//! the runtime if any plugin contributed anything. [`setup_tui_runtime`]
//! always returns a runtime for the TUI and also loads the trusted
//! `init.lua` from the user config dir. [`PluginEventHooks`] wraps
//! another `Hooks` and forwards loop events to subscribed plugin
//! handlers, plus synthesizes the `agent_start` / `agent_end` events the
//! loop never emits itself.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use kage_core::config::{KeybindingsConfig, PluginsConfig};
use kage_core::{LoopEvent, Message, ToolOutput, sync::lock};
use kage_loop::{CompactionPrep, Hooks, StreamRequest, TurnSummary};
use kage_plugin::{
    LoadReport, LogLevel, PluginRuntime, PluginRuntimeBuilder, SharedHostLog, SharedOptions,
};
use kage_provider::{Provider, ProviderRegistry};
use serde_json::json;

/// Construct a plugin runtime, load `*.lua` files from `plugins_dir`, and
/// return the runtime if at least one plugin loaded successfully. Returns
/// `Ok(None)` when the directory is missing or empty. Uses the default
/// stderr-backed sink and never loads `init.lua`.
pub fn setup_runtime(
    plugins_dir: &Path,
    workdir: &Path,
    model: &str,
    system_prompt: &str,
) -> Result<Option<Arc<PluginRuntime>>, String> {
    // Capability grants and the load allowlist come from the same
    // layered config the rest of the host reads. Fail closed: if the
    // config cannot be loaded, no plugin gets any elevated capability
    // rather than silently proceeding with an unknown grant set; the
    // error is printed so the downgrade is visible.
    let plugins_cfg = match kage_core::config::Config::load_layered(workdir) {
        Ok(c) => c.plugins,
        Err(e) => {
            eprintln!("kage: plugins: {e}; capability grants not applied");
            PluginsConfig::default()
        }
    };
    migrate_plugin_store_dir();
    let runtime = runtime_builder(plugins_cfg, workdir, model, system_prompt)
        .state_dir(crate::data_root().ok().map(|r| r.join("plugin-state")))
        .build()
        .map_err(|e| format!("plugin runtime: {e}"))?;
    let report =
        kage_plugin::load_dir(plugins_dir, &runtime).map_err(|e| format!("plugin load: {e}"))?;
    report_plugins(&report);
    if report.loaded.is_empty() {
        return Ok(None);
    }
    eprintln!(
        "kage: loaded {} plugin{} from {}",
        report.loaded.len(),
        if report.loaded.len() == 1 { "" } else { "s" },
        plugins_dir.display(),
    );
    Ok(Some(Arc::new(runtime)))
}

/// Build the TUI's runtime. Unlike [`setup_runtime`] it always returns a
/// runtime, even with no plugins, so the embedded defaults have a Lua
/// state. It loads the plugins in `plugins_dir`, the `[keybindings]`
/// table, and then the trusted `<user_dir>/init.lua`. `plugins_cfg` and
/// `keybindings` are the already loaded config tables, `options` the
/// store seeded from the same config, and `sink` receives plugin
/// output, so nothing is written to stderr while the TUI owns the
/// screen. Themes resolve against the bundled set and the user themes
/// dir, which also provide the base highlight groups.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is separately loaded startup state"
)]
pub(crate) fn setup_tui_runtime(
    plugins_dir: Option<&Path>,
    user_dir: Option<&Path>,
    plugins_cfg: PluginsConfig,
    keybindings: KeybindingsConfig,
    options: SharedOptions,
    workdir: &Path,
    model: &str,
    system_prompt: &str,
    sink: SharedHostLog,
) -> Result<Arc<PluginRuntime>, String> {
    migrate_plugin_store_dir();
    let themes_dir = crate::themes_dir().ok();
    let runtime = runtime_builder(plugins_cfg, workdir, model, system_prompt)
        .sink(sink)
        .state_dir(crate::data_root().ok().map(|r| r.join("plugin-state")))
        .user_dir(user_dir.map(Path::to_path_buf))
        .keybindings(keybindings)
        .options(options)
        .themes(Arc::new(kage_tui::theme::Themes::new(themes_dir)))
        .build()
        .map_err(|e| format!("plugin runtime: {e}"))?;
    load_tui_runtime(runtime, plugins_dir)
}

/// Run the full load on a TUI runtime and hand it back. Failures of
/// single plugins and of `init.lua` are logged through the runtime's
/// sink and do not fail the load.
fn load_tui_runtime(
    runtime: PluginRuntime,
    plugins_dir: Option<&Path>,
) -> Result<Arc<PluginRuntime>, String> {
    let report =
        kage_plugin::load_all(plugins_dir, &runtime).map_err(|e| format!("plugin load: {e}"))?;
    report_plugins(&report);
    Ok(Arc::new(runtime))
}

fn runtime_builder(
    plugins_cfg: PluginsConfig,
    workdir: &Path,
    model: &str,
    system_prompt: &str,
) -> PluginRuntimeBuilder {
    PluginRuntime::builder()
        .workdir(workdir.to_path_buf())
        .capabilities(plugins_cfg.capabilities)
        .enabled(plugins_cfg.enabled)
        .plugin_config(plugins_cfg.config)
        .credential_lookup(std::sync::Arc::new(|provider: &str| {
            crate::auth::AuthStore::load()
                .ok()
                .and_then(|store| store.access_token(provider).map(str::to_owned))
        }))
        .config(json!({
            "model": model,
            "cwd": workdir.display().to_string(),
            "system_prompt": system_prompt,
        }))
}

fn report_plugins(report: &LoadReport) {
    for (path, err) in &report.failed {
        eprintln!("kage: plugin {} failed to load: {err}", path.display());
    }
    for path in &report.skipped {
        eprintln!(
            "kage: plugin {} skipped (not in [plugins] enabled)",
            path.display()
        );
    }
}

/// Move the plugin store from its legacy location under the state root
/// (`$XDG_STATE_HOME/kage/plugin-state`) to the data root
/// (`$XDG_DATA_HOME/kage/plugin-state`). A legacy dir is migrated only
/// when the new location does not exist yet, so already-migrated
/// installs are left untouched. Loading never fails on migration: the
/// outcome is logged as a single line.
fn migrate_plugin_store_dir() {
    let (Ok(old_root), Ok(new_root)) = (crate::state_root(), crate::data_root()) else {
        return;
    };
    let old = old_root.join("plugin-state");
    let new = new_root.join("plugin-state");
    match migrate_store_dir(&old, &new) {
        Ok(true) => {
            eprintln!(
                "kage: migrated plugin store from {} to {}",
                old.display(),
                new.display()
            );
        }
        Ok(false) => {}
        Err(e) => eprintln!("kage: plugin store migration failed: {e}"),
    }
}

/// Rename `old` to `new` when `old` exists and `new` does not, returning
/// whether a migration ran. When the two locations sit on different
/// filesystems and `rename` reports `EXDEV`, fall back to a recursive
/// copy of `old` followed by its removal.
fn migrate_store_dir(old: &Path, new: &Path) -> std::io::Result<bool> {
    if !old.is_dir() || new.exists() {
        return Ok(false);
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(old, new) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_dir_recursive(old, new)?;
            std::fs::remove_dir_all(old)?;
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

/// Copy every file and subdirectory of `src` into `dst`, creating `dst`
/// as needed.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Register every provider a plugin contributed via
/// `kage.register_provider` into `registry`. A plugin can shadow a
/// built-in id (the registry overwrites prior entries with the same
/// id); we log that case so the user sees the override deliberately.
pub fn merge_plugin_providers(runtime: &PluginRuntime, registry: &mut ProviderRegistry) {
    let existing: std::collections::HashSet<String> = registry.ids().map(str::to_owned).collect();
    for provider in runtime.registered_providers() {
        let id = provider.metadata().id.clone();
        if existing.contains(&id) {
            eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
        }
        registry.register(provider);
    }
}

/// Fire `before_agent_start` and `agent_start` before a run's first turn,
/// with the system prompt and first user message in scope.
pub(crate) fn dispatch_run_start(
    rt: &PluginRuntime,
    system_prompt: &str,
    first_user_message: &str,
) {
    let payload = json!({
        "system_prompt": system_prompt,
        "first_user_message": first_user_message,
    });
    for (event, payload) in [("before_agent_start", payload), ("agent_start", json!({}))] {
        if let Err(err) = rt.dispatch_event(event, &payload) {
            log_plugin_error(rt, format_args!("{event} dispatch: {err}"));
        }
    }
}

/// Fire `agent_end` after a run returns.
pub(crate) fn dispatch_run_end(rt: &PluginRuntime, ok: bool) {
    if let Err(err) = rt.dispatch_event("agent_end", &json!({ "ok": ok })) {
        log_plugin_error(rt, format_args!("agent_end dispatch: {err}"));
    }
}

/// Queue `user_bash` after a `!` shell command ends. `exit_code` is
/// `None` when a signal ended the command.
pub(crate) fn notify_user_bash(rt: &PluginRuntime, cmd: &str, exit_code: Option<i32>) {
    let payload = json!({ "cmd": cmd, "exit_code": exit_code });
    if let Err(err) = rt.notify_event("user_bash", &payload) {
        log_plugin_error(rt, format_args!("user_bash dispatch: {err}"));
    }
}

fn log_plugin_error(rt: &PluginRuntime, args: std::fmt::Arguments<'_>) {
    let sink = rt.sink();
    lock(&sink).log(LogLevel::Error, &args.to_string());
}

/// Fire the plugin events that mirror a loop event: message, tool, and
/// turn lifecycle. Each event is skipped when nobody subscribes to it.
/// `tool_names` maps tool call ids to tool names for the run, so
/// `tool_result` carries the name its patterns match against.
pub(crate) fn forward_event(
    rt: &PluginRuntime,
    event: &LoopEvent,
    tool_names: &mut HashMap<String, String>,
) {
    let fire = |name: &str, payload: serde_json::Value| {
        let _ = rt.dispatch_event(name, &payload);
    };
    let wanted = |name: &str| rt.handler_count(name) > 0;
    match event {
        LoopEvent::MessageStart { id } if wanted("message_start") => {
            fire("message_start", json!({ "id": id.to_string() }));
        }
        LoopEvent::TextDelta { id, delta } if wanted("message_update") => {
            fire(
                "message_update",
                json!({
                    "id": id.to_string(),
                    "delta": delta,
                }),
            );
        }
        LoopEvent::MessageEnd { id, usage, .. } => {
            let end = wanted("message_end");
            let response = wanted("after_provider_response");
            if !end && !response {
                return;
            }
            let payload = json!({
                "id": id.to_string(),
                "usage": {
                    "input": usage.input,
                    "output": usage.output,
                    "cache_read": usage.cache_read,
                    "cache_write": usage.cache_write,
                },
            });
            if end {
                fire("message_end", payload.clone());
            }
            if response {
                fire("after_provider_response", payload);
            }
        }
        LoopEvent::ToolCallStart {
            id,
            name,
            input_partial,
        } => {
            tool_names.insert(id.to_string(), name.clone());
            if wanted("tool_call") {
                fire(
                    "tool_call",
                    json!({
                        "id": id.to_string(),
                        "name": name,
                        "input": input_partial,
                    }),
                );
            }
        }
        LoopEvent::ToolCallEnd { id, output } => {
            let id = id.to_string();
            let name = tool_names.remove(&id);
            if wanted("tool_result") {
                fire(
                    "tool_result",
                    json!({
                        "id": id,
                        "name": name,
                        "is_error": output.is_error,
                        "text": output.text,
                    }),
                );
            }
        }
        LoopEvent::ToolUpdate { id, update } if wanted("tool_update") => {
            fire(
                "tool_update",
                json!({
                    "id": id.to_string(),
                    "content": update.content,
                    "structured": update.structured,
                }),
            );
        }
        LoopEvent::TurnStarted { index } if wanted("turn_start") => {
            fire("turn_start", json!({ "index": index }));
        }
        LoopEvent::TurnEnded {
            index,
            had_tool_calls,
        } if wanted("turn_end") => {
            fire(
                "turn_end",
                json!({ "index": index, "had_tool_calls": had_tool_calls }),
            );
        }
        _ => {}
    }
}

/// Hooks adapter that lets plugins steer a run: context and request
/// transforms, compaction, stop predicates, and queued messages. Plugin
/// dispatch errors are logged through the runtime's host log; they never
/// abort the loop.
pub struct PluginEventHooks<H: Hooks> {
    inner: H,
    runtime: Arc<PluginRuntime>,
    /// FIFO of `kage.send_message` payloads pulled out of the runtime
    /// at most once per loop pass. The loop polls
    /// [`Hooks::get_steering`] potentially many times; we drain the
    /// runtime queue lazily into `pending_steering` so each call
    /// returns at most one message and we never lose ordering across
    /// turns.
    pending_steering: std::collections::VecDeque<String>,
}

impl<H: Hooks> PluginEventHooks<H> {
    /// Wrap `inner` so its calls flow through `runtime`'s plugin dispatch.
    pub fn new(inner: H, runtime: Arc<PluginRuntime>) -> Self {
        Self {
            inner,
            runtime,
            pending_steering: std::collections::VecDeque::new(),
        }
    }

    fn log_error(&self, args: std::fmt::Arguments<'_>) {
        log_plugin_error(&self.runtime, args);
    }
}

impl<H: Hooks> Hooks for PluginEventHooks<H> {
    fn before_tool_call(
        &mut self,
        id: &kage_core::ToolCallId,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<ToolOutput> {
        self.inner.before_tool_call(id, name, input)
    }

    fn after_tool_call(&mut self, name: &str, output: ToolOutput) -> ToolOutput {
        self.inner.after_tool_call(name, output)
    }

    fn transform_context(&mut self, messages: &mut Vec<Message>) -> Result<(), String> {
        self.inner.transform_context(messages)?;
        if self.runtime.handler_count("transform_context") == 0 {
            return Ok(());
        }
        let payload = serde_json::to_value(&*messages)
            .map_err(|e| format!("transform_context: serialize history: {e}"))?;
        let result = self
            .runtime
            .dispatch_transform("transform_context", payload)
            .map_err(|e| format!("transform_context: lua dispatch: {e}"))?;
        let next: Vec<Message> = serde_json::from_value(result)
            .map_err(|e| format!("transform_context: plugin returned invalid history: {e}"))?;
        *messages = next;
        Ok(())
    }

    fn transform_provider_request(&mut self, req: &mut StreamRequest) -> Result<(), String> {
        self.inner.transform_provider_request(req)?;
        if self.runtime.handler_count("before_provider_request") == 0 {
            return Ok(());
        }
        let payload = serde_json::to_value(&*req)
            .map_err(|e| format!("before_provider_request: serialize request: {e}"))?;
        let result = self
            .runtime
            .dispatch_transform("before_provider_request", payload)
            .map_err(|e| format!("before_provider_request: lua dispatch: {e}"))?;
        let next: StreamRequest = serde_json::from_value(result).map_err(|e| {
            format!("before_provider_request: plugin returned invalid request: {e}")
        })?;
        *req = next;
        Ok(())
    }

    fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
        self.inner.prepare_compaction(prep)?;
        if self.runtime.handler_count("compact_prepare") == 0 {
            return Ok(());
        }
        let payload = json!({
            "transcript": prep.transcript,
            "instruction": prep.instruction,
            "prompt": prep.prompt,
            "model": prep.model,
            "summarized": prep.summarized,
            "kept": prep.kept,
        });
        let result = self
            .runtime
            .dispatch_transform("compact_prepare", payload)
            .map_err(|e| format!("compact_prepare: lua dispatch: {e}"))?;
        if let Some(obj) = result.as_object() {
            if let Some(s) = obj.get("prompt").and_then(|v| v.as_str()) {
                s.clone_into(&mut prep.prompt);
            }
            if let Some(s) = obj.get("instruction").and_then(|v| v.as_str()) {
                s.clone_into(&mut prep.instruction);
            }
            if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
                prep.summary_override = Some(s.to_owned());
            }
        }
        Ok(())
    }

    fn should_stop_after_turn(&mut self, summary: &TurnSummary) -> bool {
        if self.inner.should_stop_after_turn(summary) {
            return true;
        }
        if self.runtime.handler_count("should_stop_after_turn") == 0 {
            return false;
        }
        let payload = json!({
            "index": summary.index,
            "had_tool_calls": summary.had_tool_calls,
            "usage": {
                "input": summary.usage.input,
                "output": summary.usage.output,
                "cache_read": summary.usage.cache_read,
                "cache_write": summary.usage.cache_write,
            },
        });
        match self
            .runtime
            .dispatch_predicate("should_stop_after_turn", &payload)
        {
            Ok(stop) => stop,
            Err(err) => {
                self.log_error(format_args!("should_stop_after_turn dispatch: {err}"));
                false
            }
        }
    }

    fn get_steering(&mut self) -> Option<String> {
        // Inner host hooks win when they have a steering message: a
        // CLI-issued slash command or the user's typed prompt should
        // not be overridden by a plugin's `send_message` chatter. We
        // only consult the plugin queue when the inner hook produced
        // nothing.
        if let Some(msg) = self.inner.get_steering() {
            return Some(msg);
        }
        self.drain_plugin_messages();
        self.pending_steering.pop_front()
    }

    fn get_followup(&mut self) -> Option<String> {
        if let Some(msg) = self.inner.get_followup() {
            return Some(msg);
        }
        self.drain_plugin_messages();
        self.pending_steering.pop_front()
    }
}

impl<H: Hooks> PluginEventHooks<H> {
    /// Move every queued `kage.send_message` payload from the runtime
    /// into `pending_steering`. Non-user roles are filtered out and
    /// logged because there is no synthetic-assistant or system-note
    /// delivery path. The Lua boundary already rejects those, so
    /// hitting this branch means an API expansion did not update the
    /// host side.
    fn drain_plugin_messages(&mut self) {
        for msg in self.runtime.take_pending_messages() {
            match msg.deliver_as {
                kage_plugin::PendingRole::User => self.pending_steering.push_back(msg.text),
                other => self.log_error(format_args!(
                    "send_message: dropping unsupported deliver_as {other:?}"
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_loop::NoopHooks;
    use kage_plugin::PluginRuntime;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn get_steering_returns_send_message_payload_when_inner_is_empty() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval("kage.send_message('please continue')").unwrap();
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        assert_eq!(hooks.get_steering(), Some("please continue".to_owned()));
        // Queue exhausted after one drain.
        assert_eq!(hooks.get_steering(), None);
    }

    #[test]
    fn inner_steering_takes_precedence_over_plugin_queue() {
        struct InnerOnce(Option<String>);
        impl Hooks for InnerOnce {
            fn get_steering(&mut self) -> Option<String> {
                self.0.take()
            }
        }
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval("kage.send_message('plugin says hi')").unwrap();
        let mut hooks =
            PluginEventHooks::new(InnerOnce(Some("user typed this".into())), Arc::clone(&rt));
        // First poll: inner wins.
        assert_eq!(hooks.get_steering(), Some("user typed this".to_owned()));
        // Second poll: plugin queue drains.
        assert_eq!(hooks.get_steering(), Some("plugin says hi".to_owned()));
    }

    fn sample_prep() -> CompactionPrep {
        CompactionPrep {
            transcript: "=== user ===\nhi\n".to_owned(),
            instruction: "old instruction".to_owned(),
            prompt: "old prompt".to_owned(),
            model: "mock:m".to_owned(),
            summarized: 6,
            kept: 4,
            summary_override: None,
        }
    }

    #[test]
    fn compact_prepare_summary_override_skips_model() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval(
            "kage.on('compact_prepare', function(ev)
                 return { summary = 'PLUGIN ' .. tostring(ev.summarized) }
             end)",
        )
        .unwrap();
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        let mut p = sample_prep();
        hooks.prepare_compaction(&mut p).unwrap();
        assert_eq!(p.summary_override.as_deref(), Some("PLUGIN 6"));
    }

    #[test]
    fn compact_prepare_rewrites_prompt_and_instruction() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval(
            "kage.on('compact_prepare', function(_ev)
                 return { prompt = 'NEW', instruction = 'INS' }
             end)",
        )
        .unwrap();
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        let mut p = sample_prep();
        hooks.prepare_compaction(&mut p).unwrap();
        assert_eq!(p.prompt, "NEW");
        assert_eq!(p.instruction, "INS");
        assert_eq!(p.summary_override, None);
    }

    #[test]
    fn compact_prepare_without_handler_is_passthrough() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        let mut p = sample_prep();
        hooks.prepare_compaction(&mut p).unwrap();
        assert_eq!(p.prompt, "old prompt");
        assert_eq!(p.instruction, "old instruction");
        assert_eq!(p.summary_override, None);
    }

    #[test]
    fn send_message_drains_in_fifo_order() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval("kage.send_message('first'); kage.send_message('second')")
            .unwrap();
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        assert_eq!(hooks.get_steering(), Some("first".to_owned()));
        assert_eq!(hooks.get_steering(), Some("second".to_owned()));
        assert_eq!(hooks.get_steering(), None);
    }

    #[test]
    fn store_migration_is_noop_without_legacy_dir() {
        let root = tempdir().unwrap();
        let old = root.path().join("state/plugin-state");
        let new = root.path().join("data/plugin-state");
        assert!(!migrate_store_dir(&old, &new).unwrap());
        assert!(!new.exists());
    }

    #[test]
    fn store_migration_moves_legacy_dir_with_content_intact() {
        let root = tempdir().unwrap();
        let old = root.path().join("state/plugin-state");
        let new = root.path().join("data/plugin-state");
        std::fs::create_dir_all(old.join("nested")).unwrap();
        std::fs::write(old.join("a.json"), "{\"count\":1}").unwrap();
        std::fs::write(old.join("nested/b.json"), "{}").unwrap();
        assert!(migrate_store_dir(&old, &new).unwrap());
        assert!(!old.exists());
        assert_eq!(
            std::fs::read_to_string(new.join("a.json")).unwrap(),
            "{\"count\":1}"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("nested/b.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn store_migration_skips_when_new_location_exists() {
        let root = tempdir().unwrap();
        let old = root.path().join("state/plugin-state");
        let new = root.path().join("data/plugin-state");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("a.json"), "{}").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("b.json"), "{}").unwrap();
        assert!(!migrate_store_dir(&old, &new).unwrap());
        assert!(old.join("a.json").exists());
        assert!(new.join("b.json").exists());
        assert!(!new.join("a.json").exists());
    }

    #[test]
    fn store_copy_fallback_reproduces_tree_across_distinct_roots() {
        // Two separate tempdirs stand in for the cross-device case where
        // `rename` fails with `EXDEV`; the copy helper is exercised
        // directly and the source tree removed afterwards, matching
        // `migrate_store_dir`'s fallback sequence.
        let src_root = tempdir().unwrap();
        let dst_root = tempdir().unwrap();
        let old = src_root.path().join("plugin-state");
        let new = dst_root.path().join("plugin-state");
        std::fs::create_dir_all(old.join("nested")).unwrap();
        std::fs::write(old.join("a.json"), "{\"count\":1}").unwrap();
        std::fs::write(old.join("nested/b.json"), "{}").unwrap();
        copy_dir_recursive(&old, &new).unwrap();
        std::fs::remove_dir_all(&old).unwrap();
        assert!(!old.exists());
        assert_eq!(
            std::fs::read_to_string(new.join("a.json")).unwrap(),
            "{\"count\":1}"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("nested/b.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn tui_runtime_loads_init_with_an_empty_plugins_dir() {
        let plugins = tempdir().unwrap();
        let user = tempdir().unwrap();
        let workdir = tempdir().unwrap();
        std::fs::write(
            user.path().join("init.lua"),
            "kage.register_command({ name='mine', description='', handler=function() end })",
        )
        .unwrap();
        std::fs::create_dir_all(workdir.path().join(".kage")).unwrap();
        std::fs::write(
            workdir.path().join(".kage/init.lua"),
            "kage.register_command({ name='project', description='', handler=function() end })",
        )
        .unwrap();
        let runtime = runtime_builder(PluginsConfig::default(), workdir.path(), "m", "")
            .user_dir(Some(user.path().to_path_buf()))
            .build()
            .unwrap();
        let rt = load_tui_runtime(runtime, Some(plugins.path())).unwrap();
        let names: Vec<_> = rt
            .registered_commands()
            .iter()
            .map(|c| c.name().to_owned())
            .collect();
        assert_eq!(names, ["mine"]);
    }
}
