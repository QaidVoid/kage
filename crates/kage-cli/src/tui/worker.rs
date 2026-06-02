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
                    if let Ok(mut buf) = buffer.lock() {
                        buf.push_custom("kage:mcp", format!("restarted `{name}`"), false);
                    }
                }
                Err(e) => {
                    if let Ok(mut buf) = buffer.lock() {
                        buf.push_custom("kage:error", format!("mcp restart `{name}`: {e}"), false);
                    }
                }
            }
        }
    }
    for (server, err) in manager.refresh_into(tools) {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", format!("mcp `{server}`: {err}"), false);
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn spawn_worker(cfg: WorkerConfig) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let WorkerConfig {
            registry,
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
            loop_cfg,
            steering,
            tx_self,
            plugins_dir,
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
                    if let Err(err) = crate::history::append(&text)
                        && let Ok(mut buf) = buffer.lock()
                    {
                        buf.push_custom("kage:error", format!("history: {err}"), false);
                    }
                    // Re-resolve the model on every turn so a switch
                    // request between turns takes effect immediately.
                    let qualified = active_qualified
                        .lock()
                        .expect("active model mutex poisoned")
                        .clone();
                    let resolved = match registry.resolve(&qualified) {
                        Ok(r) => r,
                        Err(e) => {
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:error",
                                    format!("model {qualified} unavailable: {e}"),
                                    false,
                                );
                            }
                            continue;
                        }
                    };
                    let provider = Arc::clone(resolved.provider);
                    let bare_model = resolved.model.clone();
                    let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
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
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.working = true;
                    }
                    let ok = run_with_hooks(
                        provider.as_ref(),
                        &tools,
                        &mut cx_guard,
                        loop_cfg,
                        &cancel,
                        &buffer,
                        plugin_runtime.as_ref(),
                        writer_for_turn,
                        &user_msg,
                        session_usage.clone(),
                        qualified.clone(),
                        context_window,
                        steering.clone(),
                    );
                    // Re-submit any prompts the user queued after the
                    // last turn boundary checked steering: the inner
                    // loop exits as soon as a turn finishes without
                    // tool calls, so a late-arriving steering item is
                    // not picked up. Resending through the channel
                    // preserves FIFO with any post-run submits and
                    // reuses the full Submit path (history, session
                    // title, etc.) without duplicating handler logic.
                    // Drains before flipping `working = false` so a
                    // simultaneous user submit cannot race ahead of
                    // earlier queued items.
                    if let Ok(mut q) = steering.lock() {
                        while let Some(text) = q.pop_front() {
                            let _ = tx_self.send(RunRequest::Submit {
                                text,
                                images: Vec::new(),
                            });
                        }
                    }
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.working = false;
                    }
                    if ok && let Err(err) = crate::state::record_last_model(&qualified) {
                        if let Ok(mut buf) = buffer.lock() {
                            buf.push_custom("kage:error", format!("state: {err}"), false);
                        }
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
                        let path = sp.lock().expect("session path mutex poisoned").clone();
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
                        match rt
                            .registered_command_overrides()
                            .into_iter()
                            .chain(rt.registered_commands())
                            .find(|c| c.name() == name)
                        {
                            Some(cmd) => {
                                if let Some(out) =
                                    run_bridged_command(rt, &cmd, &args, &dialog_tx, &buffer)
                                    && !out.text.is_empty()
                                    && let Ok(mut buf) = buffer.lock()
                                {
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
                            None => {
                                if let Ok(mut buf) = buffer.lock() {
                                    buf.push_custom(
                                        "kage:error",
                                        format!("no plugin command: {name}"),
                                        false,
                                    );
                                }
                            }
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
                        && let Ok(mut buf) = buffer.lock()
                    {
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
                    let qualified = active_qualified
                        .lock()
                        .expect("active model mutex poisoned")
                        .clone();
                    let resolved = match registry.resolve(&qualified) {
                        Ok(r) => r,
                        Err(e) => {
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:error",
                                    format!("compact: model {qualified} unavailable: {e}"),
                                    false,
                                );
                            }
                            continue;
                        }
                    };
                    let provider = Arc::clone(resolved.provider);
                    let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
                    let writer_for_turn = open_writer_for_turn(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    );
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.working = true;
                    }
                    let ran = run_compact_with_hooks(
                        provider.as_ref(),
                        &mut cx_guard,
                        &cancel,
                        &buffer,
                        plugin_runtime.as_ref(),
                        writer_for_turn,
                    );
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.working = false;
                    }
                    match ran {
                        Ok(true) => {}
                        Ok(false) => {
                            push_toast(
                                &toasts,
                                Toast::info("compact: not enough history yet".to_owned()),
                            );
                        }
                        Err(e) => {
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:error",
                                    format!("compact failed: {e}"),
                                    false,
                                );
                            }
                        }
                    }
                }
                RunRequest::CycleThinkingLevel => {
                    let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
                    let prev = cx_guard.thinking_level.unwrap_or_default();
                    let next = prev.cycle();
                    cx_guard.thinking_level = Some(next);
                    drop(cx_guard);
                    if let Ok(mut snap) = session_usage.lock() {
                        snap.thinking_level = Some(next);
                    }
                    push_toast(
                        &toasts,
                        Toast::info(format!("thinking level: {}", next.label())),
                    );
                    if let Some(rt) = plugin_runtime.as_ref() {
                        let _ = rt.dispatch_event(
                            "thinking_level_select",
                            &serde_json::json!({
                                "prev": prev.as_str(),
                                "next": next.as_str(),
                                "source": "cycle",
                            }),
                        );
                    }
                    if let Some(mut writer) = open_writer_for_turn(
                        session_path.as_ref(),
                        session_header.as_ref(),
                        &buffer,
                    ) && let Err(err) =
                        writer.append(&kage_session::SessionEntry::ThinkingLevelChange(
                            kage_session::ThinkingLevelChange {
                                id: kage_session::EntryId::new(),
                                ts: chrono::Utc::now(),
                                level: next.as_str().to_owned(),
                            },
                        ))
                        && let Ok(mut buf) = buffer.lock()
                    {
                        buf.push_custom(
                            "kage:error",
                            format!("session: append thinking_level: {err}"),
                            false,
                        );
                    }
                }
                RunRequest::SwitchModel(new_model) => {
                    // Validate before switching so a typo doesn't break
                    // the next turn silently.
                    match registry.resolve(&new_model) {
                        Ok(_) => {
                            let prev = active_qualified
                                .lock()
                                .expect("active model mutex poisoned")
                                .clone();
                            active_qualified
                                .lock()
                                .expect("active model mutex poisoned")
                                .clone_from(&new_model);
                            if let Ok(mut snap) = session_usage.lock() {
                                snap.model.clone_from(&new_model);
                            }
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
                            if let Err(err) = crate::state::record_last_model(&new_model)
                                && let Ok(mut buf) = buffer.lock()
                            {
                                buf.push_custom("kage:error", format!("state: {err}"), false);
                            }
                        }
                        Err(e) => {
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:error",
                                    format!("cannot switch to {new_model}: {e}"),
                                    false,
                                );
                            }
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
                RunRequest::ReloadPlugins => {
                    let Some(rt) = plugin_runtime.as_ref() else {
                        continue;
                    };
                    let Some(dir) = plugins_dir.as_ref() else {
                        continue;
                    };
                    match rt.reload_dir(dir) {
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
                                if let Ok(mut buf) = buffer.lock() {
                                    buf.push_custom(
                                        "kage:error",
                                        format!("plugin {}: {err}", path.display()),
                                        false,
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            if let Ok(mut buf) = buffer.lock() {
                                buf.push_custom(
                                    "kage:error",
                                    format!("plugin reload: {err}"),
                                    false,
                                );
                            }
                        }
                    }
                }
            }
            refresh_session_entries(plugin_runtime.as_ref(), session_path.as_ref());
        }
    })
}
