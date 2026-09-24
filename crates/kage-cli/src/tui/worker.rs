//! Agent-loop worker thread and per-turn driving.

#[allow(clippy::wildcard_imports)] // tui split: shares the parent module scope
use super::*;

/// Apply pending MCP changes before a turn: plugin-requested
/// restarts (`kage.mcp.restart`) first, then a hot tool-list refresh
/// for any server that announced `tools/list_changed`. Outcomes and
/// failures are surfaced inline; nothing is swallowed.
pub(crate) fn drain_mcp_updates(
    manager: &mut McpManager,
    tools: &mut ToolRegistry,
    runtime: Option<&Arc<PluginRuntime>>,
    buffer: &SharedBuffer,
) {
    if let Some(rt) = runtime {
        for name in rt.take_mcp_restarts() {
            match manager.restart(&name, tools) {
                Ok(()) => {
                    let mut buf = lock(buffer);
                    buf.push_custom("kage:mcp", format!("restarted `{name}`"), false);
                }
                Err(e) => {
                    let mut buf = lock(buffer);
                    buf.push_custom("kage:error", format!("mcp restart `{name}`: {e}"), false);
                }
            }
        }
    }
    for (server, err) in manager.refresh_into(tools) {
        let mut buf = lock(buffer);
        buf.push_custom("kage:error", format!("mcp `{server}`: {err}"), false);
    }
}

/// Drain leftover steering prompts into the worker channel and clear
/// the in-flight flag as one critical section, ending a run span
/// (`Submit` and `CompactNow` alike). Locks the steering queue before
/// the usage mutex - the same order `App::handle_submit` uses when it
/// reads the flag under the queue lock - so a submit racing the end
/// of a run either queues before this drain and is re-sent FIFO with
/// any post-run channel submits, or sees `working == false` and takes
/// the channel path. The agent loop makes no final steering check
/// when its last turn ends, so a prompt that missed the drain would
/// otherwise strand in the queue.
fn end_run_span(
    steering: &kage_tui::SharedSteering,
    session_usage: &SharedSessionUsage,
    tx_self: &mpsc::Sender<RunRequest>,
) {
    let mut q = lock(steering);
    while let Some(text) = q.pop_front() {
        let _ = tx_self.send(RunRequest::Submit {
            text,
            images: Vec::new(),
        });
    }
    lock(session_usage).working = false;
}

/// Capture a shell-escape run: combined stdout+stderr, trimmed of
/// trailing newlines, truncated to [`SHELL_OUTPUT_CAP`] chars so a
/// chatty command cannot flood the context. Returns the exit code
/// (`None` when the command was killed by a signal or failed to
/// spawn) and the truncated output.
fn run_shell_capture(cmd: &str, workdir: &std::path::Path) -> (Option<i32>, String) {
    use std::process::Command;
    const SHELL_OUTPUT_CAP: usize = 8 * 1024;
    let output = match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workdir)
        .output()
    {
        Ok(o) => o,
        Err(e) => return (None, format!("failed to run: {e}")),
    };
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    if combined.chars().count() > SHELL_OUTPUT_CAP {
        let cut: String = combined.chars().take(SHELL_OUTPUT_CAP).collect();
        combined = format!("{cut}\n... (output truncated)");
    }
    (output.status.code(), combined)
}

/// Apply a resolved thinking level to the live session: swap the
/// agent context level, sync the modeline snapshot, toast the label,
/// fire `thinking_level_select` with `source`, and persist a
/// `ThinkingLevelChange` session entry. Shared by the `Shift+Tab`
/// cycle (`source = "cycle"`) and the `:settings` dialog (`source =
/// "settings"`).
#[allow(clippy::too_many_arguments)]
fn apply_thinking_level(
    level: kage_provider::ThinkingLevel,
    source: &str,
    cx: &Arc<Mutex<AgentContext>>,
    session_usage: &SharedSessionUsage,
    toasts: &SharedToasts,
    plugin_runtime: Option<&Arc<PluginRuntime>>,
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    session_header: Option<&Arc<Mutex<Option<kage_session::Header>>>>,
    buffer: &SharedBuffer,
) {
    let prev = {
        let mut cx_guard = lock(cx);
        let prev = cx_guard.thinking_level.unwrap_or_default();
        cx_guard.thinking_level = Some(level);
        prev
    };
    lock(session_usage).thinking_level = Some(level);
    push_toast(
        toasts,
        Toast::info(format!("thinking level: {}", level.label())),
    );
    if let Some(rt) = plugin_runtime {
        let _ = rt.dispatch_event(
            "thinking_level_select",
            &serde_json::json!({
                "prev": prev.as_str(),
                "next": level.as_str(),
                "source": source,
            }),
        );
    }
    if let Some(mut writer) = open_writer_for_turn(session_path, session_header, buffer)
        && let Err(err) = writer.append(&kage_session::SessionEntry::ThinkingLevelChange(
            kage_session::ThinkingLevelChange {
                id: kage_session::EntryId::new(),
                ts: chrono::Utc::now(),
                level: level.as_str().to_owned(),
            },
        ))
    {
        let mut buf = lock(buffer);
        buf.push_custom(
            "kage:error",
            format!("session: append thinking_level: {err}"),
            false,
        );
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn spawn_worker(cfg: WorkerConfig) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let WorkerConfig {
            mut registry,
            active_qualified,
            mut tools,
            mut mcp_manager,
            cx,
            buffer,
            cancel,
            plugin_runtime,
            rx,
            session_path,
            session_header,
            session_usage,
            toasts,
            dialog_tx,
            plugin_refresh_tx,
            loop_cfg,
            steering,
            tx_self,
            plugins_dir,
            permission_gate,
        } = cfg;

        // A generated title is written at most once per session per
        // process, only for a session that began this run (no prior
        // assistant turn). Resumed sessions keep their stored title.
        let mut title_attempted = false;

        for req in rx {
            drain_mcp_updates(
                &mut mcp_manager,
                &mut tools,
                plugin_runtime.as_ref(),
                &buffer,
            );
            match req {
                RunRequest::Submit { text, images } => {
                    cancel.reset();
                    if let Err(err) = crate::history::append(&text) {
                        let mut buf = lock(&buffer);
                        buf.push_custom("kage:error", format!("history: {err}"), false);
                    }
                    // Re-resolve the model on every turn so a switch
                    // request between turns takes effect immediately.
                    let qualified = lock(&active_qualified).clone();
                    let resolved = match registry.resolve(&qualified) {
                        Ok(r) => r,
                        Err(e) => {
                            let mut buf = lock(&buffer);
                            buf.push_custom(
                                "kage:error",
                                format!("model {qualified} unavailable: {e}"),
                                false,
                            );
                            continue;
                        }
                    };
                    let provider = Arc::clone(resolved.provider);
                    let bare_model = resolved.model.clone();
                    let mut cx_guard = lock(&cx);
                    cx_guard.model = bare_model;
                    if let Some(window) =
                        crate::runtime_env::context_window_for(&registry, &qualified)
                    {
                        cx_guard.context_window = window;
                    }
                    cx_guard.max_output_tokens =
                        crate::runtime_env::max_output_tokens_for(&registry, &qualified);
                    let had_prior_assistant =
                        cx_guard.history.iter().any(|m| m.role == Role::Assistant);
                    let title_user_text = text.clone();
                    let parent = cx_guard.history.last().map(|m| m.id);
                    // Text first, then image blocks. Skip an empty
                    // text block when images are present (some
                    // providers reject empty text); keep the empty
                    // text for a bare submit so behavior is unchanged.
                    let mut content = Vec::with_capacity(1 + images.len());
                    if !text.is_empty() {
                        content.push(Content::Text { text });
                    }
                    for img in images {
                        content.push(Content::Image {
                            source: img.source,
                            mime: img.mime,
                        });
                    }
                    if content.is_empty() {
                        content.push(Content::Text {
                            text: String::new(),
                        });
                    }
                    let user_msg = Message::new(Role::User, content, parent);
                    cx_guard.history.push(user_msg.clone());
                    let writer_for_turn = open_writer_for_turn(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    );
                    let context_window = cx_guard.context_window;
                    lock(&session_usage).working = true;
                    let ok = run_with_hooks(
                        provider.as_ref(),
                        &tools,
                        &mut cx_guard,
                        loop_cfg,
                        &cancel,
                        permission_gate.clone(),
                        &buffer,
                        plugin_runtime.as_ref(),
                        writer_for_turn,
                        &user_msg,
                        session_usage.clone(),
                        qualified.clone(),
                        context_window,
                        steering.clone(),
                    );
                    // Re-submit prompts queued after the last turn
                    // boundary checked steering (the inner loop exits
                    // without a final steering check), FIFO with any
                    // post-run channel submits; also flips `working`.
                    end_run_span(&steering, &session_usage, &tx_self);
                    if ok && let Err(err) = crate::state::record_last_model(&qualified) {
                        let mut buf = lock(&buffer);
                        buf.push_custom("kage:error", format!("state: {err}"), false);
                    }
                    if ok
                        && !title_attempted
                        && !had_prior_assistant
                        && let Some(sp) = session_path.as_ref()
                    {
                        title_attempted = true;
                        let assistant_text = cx_guard
                            .history
                            .iter()
                            .rev()
                            .find(|m| m.role == Role::Assistant)
                            .and_then(first_text_of)
                            .unwrap_or_default();
                        let title_model = cx_guard.model.clone();
                        let path = lock(sp).clone();
                        drop(cx_guard);
                        write_session_title(
                            provider.as_ref(),
                            &title_model,
                            &path,
                            &title_user_text,
                            &assistant_text,
                            &cancel,
                            &buffer,
                        );
                    }
                }
                RunRequest::ResumeSession(path) => {
                    let path = match consult_session_op(
                        plugin_runtime.as_ref(),
                        "session_before_switch",
                        &path.display().to_string(),
                        &buffer,
                        &toasts,
                    ) {
                        Some(target) => PathBuf::from(target),
                        None => continue,
                    };
                    handle_resume(
                        &registry,
                        &active_qualified,
                        &cx,
                        &buffer,
                        session_path.as_ref(),
                        &session_usage,
                        &toasts,
                        &path,
                    );
                }
                RunRequest::InvokePluginCommand { name, args } => {
                    if let Some(rt) = plugin_runtime.as_ref() {
                        // Overrides are searched first so an
                        // `override_command` shadowing a built-in
                        // wins; then regular registrations.
                        if let Some(cmd) = rt
                            .registered_command_overrides()
                            .into_iter()
                            .chain(rt.registered_commands())
                            .find(|c| c.name() == name)
                        {
                            if let Some(out) =
                                run_bridged_command(rt, &cmd, &args, &dialog_tx, &buffer)
                                && !out.text.is_empty()
                            {
                                let mut buf = lock(&buffer);
                                buf.push_custom(
                                    if out.is_error {
                                        "kage:error"
                                    } else {
                                        "kage:plugin"
                                    },
                                    out.text,
                                    false,
                                );
                            }
                        } else {
                            let mut buf = lock(&buffer);
                            buf.push_custom(
                                "kage:error",
                                format!("no plugin command: {name}"),
                                false,
                            );
                        }
                    }
                }
                RunRequest::InvokePluginKeybinding { chord } => {
                    if let Some(rt) = plugin_runtime.as_ref()
                        && let Some(kb) = rt
                            .registered_keybindings()
                            .into_iter()
                            .find(|kb| kb.chord() == chord)
                        && let Some(out) = run_bridged_keybinding(rt, &kb, &dialog_tx, &buffer)
                        && !out.text.is_empty()
                    {
                        let mut buf = lock(&buffer);
                        buf.push_custom(
                            if out.is_error {
                                "kage:error"
                            } else {
                                "kage:plugin"
                            },
                            out.text,
                            false,
                        );
                    }
                }
                RunRequest::Cancel => cancel.cancel(),
                RunRequest::ForkSession { at } => {
                    let Some(at) = consult_session_op(
                        plugin_runtime.as_ref(),
                        "session_before_fork",
                        &at,
                        &buffer,
                        &toasts,
                    ) else {
                        continue;
                    };
                    let _ = handle_plugin_fork(session_path.as_ref(), &buffer, &toasts, &at);
                }
                RunRequest::CloneSession => {
                    handle_clone(session_path.as_ref(), &buffer, &toasts);
                }
                RunRequest::NewSession => {
                    handle_new(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &cx,
                        &active_qualified,
                        &session_usage,
                        &buffer,
                        &toasts,
                    );
                }
                RunRequest::ExportSession(dest) => {
                    handle_export(session_path.as_ref(), dest, &buffer, &toasts);
                }
                RunRequest::ForkSessionFile(path) => {
                    handle_fork_file(&path, &buffer, &toasts);
                }
                RunRequest::DeleteSession(path) => {
                    handle_delete_session(&path, session_path.as_ref(), &buffer, &toasts);
                }
                RunRequest::CompactNow => {
                    cancel.reset();
                    let qualified = lock(&active_qualified).clone();
                    let resolved = match registry.resolve(&qualified) {
                        Ok(r) => r,
                        Err(e) => {
                            let mut buf = lock(&buffer);
                            buf.push_custom(
                                "kage:error",
                                format!("compact: model {qualified} unavailable: {e}"),
                                false,
                            );
                            continue;
                        }
                    };
                    let provider = Arc::clone(resolved.provider);
                    let mut cx_guard = lock(&cx);
                    let writer_for_turn = open_writer_for_turn(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    );
                    lock(&session_usage).working = true;
                    let ran = run_compact_with_hooks(
                        provider.as_ref(),
                        &mut cx_guard,
                        &cancel,
                        permission_gate.clone(),
                        &buffer,
                        plugin_runtime.as_ref(),
                        writer_for_turn,
                    );
                    end_run_span(&steering, &session_usage, &tx_self);
                    match ran {
                        Ok(true) => {}
                        Ok(false) => {
                            push_toast(
                                &toasts,
                                Toast::info("compact: not enough history yet".to_owned()),
                            );
                        }
                        Err(e) => {
                            let mut buf = lock(&buffer);
                            buf.push_custom("kage:error", format!("compact failed: {e}"), false);
                        }
                    }
                }
                RunRequest::CycleThinkingLevel => {
                    let next = lock(&cx).thinking_level.unwrap_or_default().cycle();
                    apply_thinking_level(
                        next,
                        "cycle",
                        &cx,
                        &session_usage,
                        &toasts,
                        plugin_runtime.as_ref(),
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    );
                }
                RunRequest::RunShell(cmd) => {
                    let workdir = lock(&cx).workdir.clone();
                    let (code, output) = run_shell_capture(&cmd, &workdir);
                    let exit_note = code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
                    {
                        let mut buf = lock(&buffer);
                        buf.push_custom(
                            "kage:shell",
                            format!(
                                "$ {cmd}\n{output}\n(exit code {exit_note})",
                                output = output.trim_end()
                            ),
                            false,
                        );
                    }
                    // Let the model see the run on the next turn:
                    // a `[shell]` user message appended to the live
                    // history. Not recorded to the session file, so
                    // a resumed session does not carry it.
                    let parent = lock(&cx).history.last().map(|m| m.id);
                    let body = format!(
                        "[shell] ran `{cmd}` in the session working directory; \
                         exit code {exit_note}:\n{output}",
                        output = output.trim_end()
                    );
                    lock(&cx).history.push(Message::new(
                        Role::User,
                        vec![Content::Text { text: body }],
                        parent,
                    ));
                }
                RunRequest::SetThinkingLevel(value) => {
                    if let Some(level) = kage_provider::ThinkingLevel::parse(&value) {
                        apply_thinking_level(
                            level,
                            "settings",
                            &cx,
                            &session_usage,
                            &toasts,
                            plugin_runtime.as_ref(),
                            session_path.as_ref(),
                            session_header.as_ref(),
                            &buffer,
                        );
                    } else {
                        let mut buf = lock(&buffer);
                        buf.push_custom(
                            "kage:error",
                            format!("settings: unknown thinking level: {value}"),
                            false,
                        );
                    }
                }
                RunRequest::SetPermissionMode(mode) => {
                    let prev = permission_gate.mode();
                    permission_gate.set_mode(mode);
                    lock(&session_usage).permission_mode = mode;
                    let label = |m: Option<kage_core::permissions::PermissionAction>| match m {
                        Some(kage_core::permissions::PermissionAction::Ask) => "ask".to_owned(),
                        Some(kage_core::permissions::PermissionAction::Deny) => "deny".to_owned(),
                        _ => "default".to_owned(),
                    };
                    push_toast(
                        &toasts,
                        Toast::info(format!(
                            "permission mode: {}",
                            if mode.is_some() {
                                label(mode)
                            } else {
                                "default (configured rules)".to_owned()
                            }
                        )),
                    );
                    if let Some(rt) = plugin_runtime.as_ref() {
                        let _ = rt.dispatch_event(
                            "permission_mode_select",
                            &serde_json::json!({
                                "prev": label(prev),
                                "next": label(mode),
                                "source": "command",
                            }),
                        );
                    }
                }
                RunRequest::SwitchModel(new_model) => {
                    // Validate before switching so a typo doesn't break
                    // the next turn silently.
                    match registry.resolve(&new_model) {
                        Ok(_) => {
                            let prev = lock(&active_qualified).clone();
                            lock(&active_qualified).clone_from(&new_model);
                            lock(&session_usage).model.clone_from(&new_model);
                            push_toast(&toasts, Toast::info(format!("switched to {new_model}")));
                            if let Some(rt) = plugin_runtime.as_ref() {
                                let _ = rt.dispatch_event(
                                    "model_select",
                                    &serde_json::json!({
                                        "prev": prev,
                                        "next": new_model,
                                        "source": "set",
                                    }),
                                );
                            }
                            if let Err(err) = crate::state::record_last_model(&new_model) {
                                let mut buf = lock(&buffer);
                                buf.push_custom("kage:error", format!("state: {err}"), false);
                            }
                        }
                        Err(e) => {
                            let mut buf = lock(&buffer);
                            buf.push_custom(
                                "kage:error",
                                format!("cannot switch to {new_model}: {e}"),
                                false,
                            );
                        }
                    }
                }
                RunRequest::SwitchSession(target) => match target {
                    SwitchTarget::Session(s) => {
                        let path = match resolve_switch_target(&s) {
                            Ok(p) => p,
                            Err(e) => {
                                push_error(&buffer, &format!("switch: {e}"));
                                continue;
                            }
                        };
                        let Some(dest) = consult_session_op(
                            plugin_runtime.as_ref(),
                            "session_before_switch",
                            &path.display().to_string(),
                            &buffer,
                            &toasts,
                        ) else {
                            continue;
                        };
                        handle_resume(
                            &registry,
                            &active_qualified,
                            &cx,
                            &buffer,
                            session_path.as_ref(),
                            &session_usage,
                            &toasts,
                            std::path::Path::new(&dest),
                        );
                    }
                    SwitchTarget::PendingFork(at) => {
                        let Some(at) = consult_session_op(
                            plugin_runtime.as_ref(),
                            "session_before_switch",
                            &at,
                            &buffer,
                            &toasts,
                        ) else {
                            continue;
                        };
                        let Some(new_path) =
                            handle_plugin_fork(session_path.as_ref(), &buffer, &toasts, &at)
                        else {
                            continue;
                        };
                        handle_resume(
                            &registry,
                            &active_qualified,
                            &cx,
                            &buffer,
                            session_path.as_ref(),
                            &session_usage,
                            &toasts,
                            &new_path,
                        );
                    }
                },
                RunRequest::RefreshProviders => {
                    let mut fresh = crate::build_provider_registry();
                    if let Some(rt) = plugin_runtime.as_ref() {
                        crate::plugins::merge_plugin_providers(rt, &mut fresh);
                    }
                    let active = lock(&active_qualified).clone();
                    let active_ok = fresh.resolve(&active).is_ok();
                    registry = Arc::new(fresh);
                    let models = crate::tui::session_ops::available_model_items(&registry, &active);
                    let _ = plugin_refresh_tx.send(PluginRefresh {
                        commands: plugin_runtime
                            .as_ref()
                            .map(|rt| snapshot_plugin_commands(rt))
                            .unwrap_or_default(),
                        widgets: plugin_runtime
                            .as_ref()
                            .map(|rt| rt.registered_widgets())
                            .unwrap_or_default(),
                        models,
                    });
                    if active_ok {
                        push_toast(&toasts, Toast::info("providers refreshed"));
                    } else {
                        push_toast(
                            &toasts,
                            Toast::info("providers refreshed; pick a model (:model)"),
                        );
                    }
                }
                RunRequest::ReloadPlugins => {
                    let Some(rt) = plugin_runtime.as_ref() else {
                        continue;
                    };
                    let Some(dir) = plugins_dir.as_ref() else {
                        continue;
                    };
                    let reload = rt.reload_dir(dir);
                    // `reload_dir` cleared the runtime's registrations
                    // before replaying; republish whatever the fresh
                    // snapshot holds (also on error, which leaves a
                    // partially-replayed runtime) so the `:` palette
                    // and status widgets track the reload.
                    let models = crate::tui::session_ops::available_model_items(
                        &registry,
                        &lock(&active_qualified).clone(),
                    );
                    let _ = plugin_refresh_tx.send(PluginRefresh {
                        commands: snapshot_plugin_commands(rt),
                        widgets: rt.registered_widgets(),
                        models,
                    });
                    match reload {
                        Ok(report) => {
                            let msg = if report.failed.is_empty() {
                                format!("plugins reloaded ({} loaded)", report.loaded.len())
                            } else {
                                format!(
                                    "plugins reloaded ({} ok, {} failed)",
                                    report.loaded.len(),
                                    report.failed.len()
                                )
                            };
                            push_toast(
                                &toasts,
                                kage_tui::Toast::with_kind(
                                    msg,
                                    kage_tui::ToastKind::Info,
                                    kage_tui::DEFAULT_TOAST_DURATION,
                                ),
                            );
                            for (path, err) in report.failed {
                                let mut buf = lock(&buffer);
                                buf.push_custom(
                                    "kage:error",
                                    format!("plugin {}: {err}", path.display()),
                                    false,
                                );
                            }
                        }
                        Err(err) => {
                            let mut buf = lock(&buffer);
                            buf.push_custom("kage:error", format!("plugin reload: {err}"), false);
                        }
                    }
                }
            }
            refresh_session_entries(plugin_runtime.as_ref(), session_path.as_ref());
        }
    })
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn run_shell_capture_combines_streams_and_exit_code() {
        let dir = std::env::temp_dir();
        let (code, out) = run_shell_capture("echo out; echo err >&2", &dir);
        assert_eq!(code, Some(0));
        assert!(out.contains("out"), "{out}");
        assert!(out.contains("err"), "{out}");
    }

    #[test]
    fn run_shell_capture_reports_failure_and_signal() {
        let dir = std::env::temp_dir();
        let (code, out) = run_shell_capture("exit 3", &dir);
        assert_eq!(code, Some(3));
        assert_eq!(out, "");
        let (code, _) = run_shell_capture("kill -9 $$", &dir);
        assert_eq!(code, None);
    }

    #[test]
    fn run_shell_capture_truncates_large_output() {
        let dir = std::env::temp_dir();
        let (_, out) = run_shell_capture("yes | head -c 100000", &dir);
        assert!(
            out.chars().count() <= 8 * 1024 + 64,
            "truncated, len {}",
            out.chars().count()
        );
        assert!(
            out.contains("output truncated"),
            "{:?}",
            &out[..out.len().min(200)]
        );
    }

    #[test]
    fn run_shell_capture_runs_in_the_given_workdir() {
        let dir = std::env::temp_dir();
        let (_, out) = run_shell_capture("pwd", &dir);
        assert!(out.trim().starts_with(dir.to_str().unwrap()), "{out}");
    }
}
