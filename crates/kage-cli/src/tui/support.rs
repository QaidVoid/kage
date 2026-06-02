//! Session title, bridge, dialog, and listing helpers.

#[allow(clippy::wildcard_imports)] // tui split: shares the parent module scope
use super::*;

/// First text block of a message, if any. Used to pull the first
/// assistant reply out of history for the title prompt.
pub(crate) fn first_text_of(m: &Message) -> Option<String> {
    m.content.iter().find_map(|c| match c {
        Content::Text { text } => Some(text.clone()),
        _ => None,
    })
}

/// Generate a session title from the first exchange and append it as
/// a [`kage_session::SessionEntry::Title`]. A file error is surfaced
/// in the buffer, never swallowed; a model failure already degraded
/// to the heuristic inside [`crate::title::generate`].
pub(crate) fn write_session_title(
    provider: &dyn kage_provider::Provider,
    model: &str,
    path: &std::path::Path,
    user_text: &str,
    assistant_text: &str,
    cancel: &CancelFlag,
    buffer: &SharedBuffer,
) {
    let title = crate::title::generate(provider, model, user_text, assistant_text, cancel);
    let entry = kage_session::SessionEntry::Title(kage_session::SessionTitle {
        id: kage_session::EntryId::new(),
        ts: chrono::Utc::now(),
        title,
    });
    match SessionWriter::open(path) {
        Ok(mut w) => {
            if let Err(e) = w.append(&entry)
                && let Ok(mut buf) = buffer.lock()
            {
                buf.push_custom("kage:error", format!("session title: {e}"), false);
            }
        }
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("session title: {e}"), false);
            }
        }
    }
}

/// Open or create the session file for the duration of one turn.
///
/// The TUI plans a session id+path at startup but defers writing the
/// header file until the first prompt actually lands. If the path
/// already exists (resumed session, or this is a follow-up turn) we
/// open it in append mode. If not, we consume the planned header,
/// create the file with it, and let subsequent turns hit the open
/// branch.
pub(crate) fn open_writer_for_turn(
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    session_header: Option<&Arc<Mutex<Option<kage_session::Header>>>>,
    buffer: &SharedBuffer,
) -> Option<SessionWriter> {
    let path_arc = session_path?;
    let path = path_arc
        .lock()
        .expect("session path mutex poisoned")
        .clone();
    if !path.exists() {
        let header =
            session_header.and_then(|h| h.lock().expect("session header mutex poisoned").take())?;
        return match SessionWriter::create(path.clone(), header) {
            Ok(w) => Some(w),
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom(
                        "kage:error",
                        format!("session: create {}: {e}", path.display()),
                        false,
                    );
                }
                None
            }
        };
    }
    match SessionWriter::open(&path) {
        Ok(w) => Some(w),
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    format!("session: open {}: {e}", path.display()),
                    false,
                );
            }
            None
        }
    }
}

/// Run a plugin command through the coroutine bridge so its handler
/// may call blocking `kage.ui.*` dialogs. Drives the suspend/resume
/// loop to completion and returns the command's output (an error
/// output on any failure), or `None` when the handler produced
/// nothing.
pub(crate) fn run_bridged_command(
    rt: &PluginRuntime,
    cmd: &kage_plugin::LuaCommand,
    raw: &str,
    dialog_tx: &mpsc::Sender<PluginDialog>,
    buffer: &SharedBuffer,
) -> Option<CommandOutput> {
    let label = format!("command {}", cmd.name());
    let prep = match cmd.prepare_bridge(raw, &serde_json::Value::Null) {
        Ok(prep) => prep,
        Err(e) => return Some(error_output(&label, &e.to_string())),
    };
    let bargs = match prep {
        BridgePrep::Ready(bargs) => bargs,
        BridgePrep::ArgError(out) => return Some(out),
    };
    let step = match rt.bridge_call(&bargs.handler, &bargs.args) {
        Ok(step) => step,
        Err(e) => return Some(error_output(&label, &e.to_string())),
    };
    drive_bridge(rt, &label, step, dialog_tx, buffer)
}

/// Run a plugin keybinding's handler through the coroutine bridge,
/// same servicing path as a command (so the handler may open
/// `kage.ui.*` dialogs). A non-empty return value is surfaced as a
/// conversation block, just like a command's output.
pub(crate) fn run_bridged_keybinding(
    rt: &PluginRuntime,
    kb: &kage_plugin::LuaKeybinding,
    dialog_tx: &mpsc::Sender<PluginDialog>,
    buffer: &SharedBuffer,
) -> Option<CommandOutput> {
    let label = format!("keybinding {}", kb.chord());
    let handler = match kb.handler() {
        Ok(handler) => handler,
        Err(e) => return Some(error_output(&label, &e.to_string())),
    };
    let step = match rt.bridge_call(&handler, &[]) {
        Ok(step) => step,
        Err(e) => return Some(error_output(&label, &e.to_string())),
    };
    drive_bridge(rt, &label, step, dialog_tx, buffer)
}

/// Drive a started bridge call to completion: service each suspend
/// through the App's dialog channel, resume/cancel, and on a terminal
/// `Done` map the value to a [`CommandOutput`]. Shared by the command
/// and keybinding paths.
pub(crate) fn drive_bridge(
    rt: &PluginRuntime,
    label: &str,
    mut step: BridgeStep,
    dialog_tx: &mpsc::Sender<PluginDialog>,
    buffer: &SharedBuffer,
) -> Option<CommandOutput> {
    loop {
        match step {
            BridgeStep::Done(value) => return Some(CommandOutput::from_json(&value)),
            BridgeStep::Suspended(req) => {
                let resumed = match service_dialog(&req, dialog_tx, buffer) {
                    Some(value) => rt.bridge_resume(&value),
                    None => rt.bridge_cancel(),
                };
                step = match resumed {
                    Ok(step) => step,
                    Err(e) => {
                        let _ = rt.bridge_abort();
                        return Some(error_output(label, &e.to_string()));
                    }
                };
            }
        }
    }
}

/// Service one suspended dialog request (`ui.select` / `ui.confirm`).
/// Builds the matching [`PluginDialog`], hands it to the App, and
/// blocks on the reply. Returns the value to resume the coroutine
/// with, or `None` (resume with `nil`) on dismissal, an unsupported
/// kind, or a malformed payload (the latter two also logged).
pub(crate) fn service_dialog(
    req: &kage_plugin::SuspendRequest,
    dialog_tx: &mpsc::Sender<PluginDialog>,
    buffer: &SharedBuffer,
) -> Option<serde_json::Value> {
    let (reply_tx, reply_rx) = mpsc::channel();
    let dialog = match req.kind.as_str() {
        "ui.select" => match SelectRequest::from_payload(&req.payload) {
            Ok(sel) => PluginDialog::Select {
                title: sel.title,
                items: sel.items,
                reply: reply_tx,
            },
            Err(e) => {
                push_error(buffer, &format!("ui.select: {e}"));
                return None;
            }
        },
        "ui.confirm" => match ConfirmRequest::from_payload(&req.payload) {
            Ok(c) => PluginDialog::Confirm {
                title: c.title,
                message: c.message,
                reply: reply_tx,
            },
            Err(e) => {
                push_error(buffer, &format!("ui.confirm: {e}"));
                return None;
            }
        },
        "ui.input" => match InputRequest::from_payload(&req.payload) {
            Ok(i) => PluginDialog::Input {
                title: i.title,
                placeholder: i.placeholder,
                reply: reply_tx,
            },
            Err(e) => {
                push_error(buffer, &format!("ui.input: {e}"));
                return None;
            }
        },
        "ui.editor" => match EditorRequest::from_payload(&req.payload) {
            Ok(ed) => PluginDialog::Editor {
                title: ed.title,
                prefill: ed.prefill,
                reply: reply_tx,
            },
            Err(e) => {
                push_error(buffer, &format!("ui.editor: {e}"));
                return None;
            }
        },
        other => {
            push_error(buffer, &format!("unsupported plugin dialog: {other}"));
            return None;
        }
    };
    if dialog_tx.send(dialog).is_err() {
        return None;
    }
    reply_rx.recv().unwrap_or(None)
}

/// Build a one-line error [`CommandOutput`] for a failed plugin
/// invocation. `label` reads like `command foo` or `keybinding ctrl+g`.
pub(crate) fn error_output(label: &str, msg: &str) -> CommandOutput {
    CommandOutput {
        text: format!("plugin {label}: {msg}"),
        is_error: true,
        structured: None,
    }
}

/// Push a `kage:error` block into the conversation buffer.
pub(crate) fn push_error(buffer: &SharedBuffer, msg: &str) {
    if let Ok(mut buf) = buffer.lock() {
        buf.push_custom("kage:error", msg.to_owned(), false);
    }
}

/// Consult plugin handlers for a session-op event before running an
/// action. Returns the (possibly patched) target string if the action
/// should proceed, or `None` if a plugin vetoed.
///
/// On veto, this also pushes a toast plus an inline error block so the
/// user sees the reason without diving into logs. With no plugin runtime
/// or no subscribers, returns `Some(target.to_owned())` immediately.
pub(crate) fn consult_session_op(
    runtime: Option<&Arc<PluginRuntime>>,
    event: &str,
    target: &str,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
) -> Option<String> {
    let Some(rt) = runtime else {
        return Some(target.to_owned());
    };
    if rt.handler_count(event) == 0 {
        return Some(target.to_owned());
    }
    match rt.dispatch_session_op(event, target) {
        Ok(kage_plugin::SessionOpDecision::Proceed) => Some(target.to_owned()),
        Ok(kage_plugin::SessionOpDecision::Patch(next)) => Some(next),
        Ok(kage_plugin::SessionOpDecision::Cancel { reason }) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("{event}: {reason}"), false);
            }
            push_toast(
                toasts,
                Toast::info(format!("session op cancelled: {reason}")),
            );
            None
        }
        Err(err) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    format!("{event}: plugin dispatch failed: {err}"),
                    false,
                );
            }
            Some(target.to_owned())
        }
    }
}

/// Extract the first text block from a user message, joined with newlines
/// if there are multiple. Returns an empty string when the message carries
/// no text (image-only, tool-result-only, etc.).
pub(crate) fn first_user_text(msg: &Message) -> String {
    let mut out = String::new();
    for block in &msg.content {
        if let Content::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// Run the agent loop with the right hook chain: TUI display innermost,
/// optional session recording in the middle, optional plugin dispatch
/// outermost, plus the [`UsageHooks`] wrapper at the very edge so the
/// modeline updates every `MessageEnd`. Returns whether the loop
/// completed successfully so the caller knows whether to bump
/// `last_model` state.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_with_hooks(
    provider: &dyn kage_provider::Provider,
    tools: &ToolRegistry,
    cx: &mut AgentContext,
    loop_cfg: LoopConfig,
    cancel: &CancelFlag,
    buffer: &SharedBuffer,
    plugin_runtime: Option<&Arc<PluginRuntime>>,
    writer: Option<SessionWriter>,
    user_msg: &Message,
    session_usage: SharedSessionUsage,
    qualified_model: String,
    context_window: u64,
    steering: kage_tui::SharedSteering,
) -> bool {
    use crate::usage_hooks::UsageHooks;
    let tui_hooks = TuiHooks::new(NoopHooks, buffer.clone()).with_steering(steering);
    match (writer, plugin_runtime) {
        (Some(w), Some(rt)) => {
            let mut recorded =
                SessionRecordingHooks::new(tui_hooks, w).with_plugin_runtime(Arc::clone(rt));
            recorded.record_user_message(user_msg);
            let plugin_hooks = PluginEventHooks::new(recorded, Arc::clone(rt));
            plugin_hooks.dispatch_before_agent_start(&cx.system_prompt, &first_user_text(user_msg));
            plugin_hooks.dispatch_agent_start();
            let mut wrapped =
                UsageHooks::new(plugin_hooks, session_usage, qualified_model, context_window);
            let res = run(provider, tools, cx, loop_cfg, &mut wrapped, cancel, |_| {});
            wrapped.into_inner().dispatch_agent_end(res.is_ok());
            res.is_ok()
        }
        (Some(w), None) => {
            let mut recorded = SessionRecordingHooks::new(tui_hooks, w);
            recorded.record_user_message(user_msg);
            let mut wrapped =
                UsageHooks::new(recorded, session_usage, qualified_model, context_window);
            run(provider, tools, cx, loop_cfg, &mut wrapped, cancel, |_| {}).is_ok()
        }
        (None, Some(rt)) => {
            let plugin_hooks = PluginEventHooks::new(tui_hooks, Arc::clone(rt));
            plugin_hooks.dispatch_before_agent_start(&cx.system_prompt, &first_user_text(user_msg));
            plugin_hooks.dispatch_agent_start();
            let mut wrapped =
                UsageHooks::new(plugin_hooks, session_usage, qualified_model, context_window);
            let res = run(provider, tools, cx, loop_cfg, &mut wrapped, cancel, |_| {});
            wrapped.into_inner().dispatch_agent_end(res.is_ok());
            res.is_ok()
        }
        (None, None) => {
            let mut wrapped =
                UsageHooks::new(tui_hooks, session_usage, qualified_model, context_window);
            run(provider, tools, cx, loop_cfg, &mut wrapped, cancel, |_| {}).is_ok()
        }
    }
}

/// Run an unconditional compaction pass through the same hook stack
/// `run_with_hooks` uses, so the resulting `LoopEvent::Compaction`
/// is mirrored to the buffer and recorded to the session file. The
/// usage hook is intentionally skipped: compaction does not change
/// the active model or window, just the saved budget.
pub(crate) fn run_compact_with_hooks(
    provider: &dyn kage_provider::Provider,
    cx: &mut AgentContext,
    cancel: &CancelFlag,
    buffer: &SharedBuffer,
    plugin_runtime: Option<&Arc<PluginRuntime>>,
    writer: Option<SessionWriter>,
) -> Result<bool, kage_core::LoopError> {
    let tui_hooks = TuiHooks::new(NoopHooks, buffer.clone());
    match (writer, plugin_runtime) {
        (Some(w), Some(rt)) => {
            let recorded =
                SessionRecordingHooks::new(tui_hooks, w).with_plugin_runtime(Arc::clone(rt));
            let mut plugin_hooks = PluginEventHooks::new(recorded, Arc::clone(rt));
            force_compact(cx, provider, cancel, &mut plugin_hooks, &mut |_| {})
        }
        (Some(w), None) => {
            let mut recorded = SessionRecordingHooks::new(tui_hooks, w);
            force_compact(cx, provider, cancel, &mut recorded, &mut |_| {})
        }
        (None, Some(rt)) => {
            let mut plugin_hooks = PluginEventHooks::new(tui_hooks, Arc::clone(rt));
            force_compact(cx, provider, cancel, &mut plugin_hooks, &mut |_| {})
        }
        (None, None) => {
            let mut hooks = tui_hooks;
            force_compact(cx, provider, cancel, &mut hooks, &mut |_| {})
        }
    }
}

/// Build the picker rows for the `Ctrl+R` session picker. Listing
/// happens at picker-open time so newly recorded sessions appear
/// without needing to restart the TUI. Unless `all` is set, only
/// sessions whose recorded `cwd` is `workdir` are shown (the picker
/// default); the in-picker `Ctrl+A` toggle re-lists with `all`.
pub(crate) fn list_session_choices(
    dir: &std::path::Path,
    workdir: &std::path::Path,
    all: bool,
) -> Vec<PickItem> {
    let Ok(mut summaries) = kage_session::list(dir) else {
        return Vec::new();
    };
    if !all {
        summaries.retain(|s| s.cwd == workdir);
    }
    // Order by last activity, newest first, so the date sections are
    // contiguous (the rows are grouped by `updated_at`'s day) and the
    // time column reads top-to-bottom within each day. `list` sorts
    // by `created_at`, which would split a day whose session was
    // resumed later.
    summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    summaries
        .into_iter()
        .map(|s| {
            let day = relative_day(s.updated_at);
            let time = s.updated_at.format("%H:%M").to_string();
            PickItem::simple(s.path.to_string_lossy().into_owned())
                .with_label(format_session_label(&s))
                .with_group(day)
                .with_right(time)
        })
        .collect()
}

/// Build the `:tree` forest rows from the sessions directory, marking
/// whichever file the runtime is currently writing as the active one.
pub(crate) fn list_session_nodes(
    dir: &std::path::Path,
    session_path: Option<&Arc<Mutex<PathBuf>>>,
) -> Vec<kage_tui::SessionNode> {
    let current = session_path.and_then(|sp| sp.lock().ok().map(|g| g.clone()));
    let Ok(summaries) = kage_session::list(dir) else {
        return Vec::new();
    };
    summaries
        .into_iter()
        .map(|s| {
            let is_current = current.as_deref() == Some(s.path.as_path());
            kage_tui::SessionNode {
                id: s.id.to_string(),
                path: s.path.to_string_lossy().into_owned(),
                parent: s.parent_session.map(|p| p.to_string()),
                label: format_session_label(&s),
                is_current,
            }
        })
        .collect()
}

/// The session picker row's label: just the title, cleaned to one
/// line. Prefers the generated title; falls back to the first line
/// of the last user prompt for sessions written before titles
/// existed. The picker right-aligns the time and truncates this to
/// fit, so no padding is baked in here.
pub(crate) fn format_session_label(s: &SessionSummary) -> String {
    s.title
        .as_deref()
        .or(s.last_user_prompt.as_deref())
        .map_or_else(
            || "(untitled session)".to_owned(),
            |t| {
                let one_line = t.replace('\n', " ");
                one_line.split_whitespace().collect::<Vec<_>>().join(" ")
            },
        )
}

/// A short, human day label relative to now: `Today` / `Yesterday`
/// for the last two days, otherwise `YYYY-MM-DD`. Drives the
/// at-a-glance grouping in the session picker.
pub(crate) fn relative_day(ts: chrono::DateTime<chrono::Utc>) -> String {
    let today = chrono::Utc::now().date_naive();
    let day = ts.date_naive();
    match (today - day).num_days() {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        _ => day.format("%Y-%m-%d").to_string(),
    }
}

/// Bridge a plugin runtime arg-spec entry over to the TUI's owned
/// arg-spec enum so [`kage_tui::App::set_plugin_commands`] can leak
/// it into a `&'static ArgSpec` for the completion engine.
pub(crate) fn translate_plugin_arg(
    arg: &kage_plugin::PluginArgSpec,
) -> kage_tui::command::OwnedArgSpec {
    use kage_plugin::PluginArgSpec as P;
    use kage_tui::command::OwnedArgSpec as O;
    match arg {
        P::Text {
            name,
            optional,
            hint,
        } => O::Text {
            name: name.clone(),
            optional: *optional,
            hint: hint.clone(),
        },
        P::Choice {
            name,
            values,
            optional,
        } => O::Choice {
            name: name.clone(),
            values: values.clone(),
            optional: *optional,
        },
        P::Path { name, optional } => O::Path {
            name: name.clone(),
            optional: *optional,
        },
        P::Session { name, optional } => O::Session {
            name: name.clone(),
            optional: *optional,
        },
        P::Flag { name } => O::Flag { name: name.clone() },
    }
}

/// Replay `path` into the live TUI: clear the buffer, replace
/// `cx.history` with the recorded one, point the worker at this file
/// for future appends, and repopulate the buffer so the user sees the
/// prior conversation rendered with the current TUI styling.
///
/// If the session's recorded model is no longer resolvable (provider
/// not authed in this run, model removed from the catalog), the
/// resume keeps the currently active model rather than failing. The
/// replay history still loads so the substitute model continues with
/// full context; a toast flags the substitution.
/// Read the JSONL session at `path` and return the id of its final
/// non-header entry, or `None` if the file holds only a header.
pub(crate) fn find_last_entry(
    path: &std::path::Path,
) -> Result<Option<kage_session::EntryId>, kage_session::SessionError> {
    let reader = SessionReader::iter(path)?;
    let mut last = None;
    for item in reader {
        let entry = item?;
        if !matches!(entry, kage_session::SessionEntry::Header(_)) {
            last = Some(entry.id());
        }
    }
    Ok(last)
}

/// Resolve a `kage.session.switch` argument to a session file path.
/// Accepts either a path string (as handed out by
/// `kage.session.list()`) or a session-id prefix, which is matched
/// against the sessions directory. A prefix that matches no session,
/// or more than one, is an error rather than a silent pick.
pub(crate) fn resolve_switch_target(target: &str) -> Result<PathBuf, String> {
    let direct = PathBuf::from(target);
    if direct.is_file() {
        return Ok(direct);
    }
    let dir = crate::sessions_dir().map_err(|e| format!("sessions dir: {e}"))?;
    let summaries = kage_session::list(&dir).map_err(|e| format!("listing sessions: {e}"))?;
    let mut hits: Vec<PathBuf> = summaries
        .into_iter()
        .filter(|s| s.id.to_string().starts_with(target))
        .map(|s| s.path)
        .collect();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(format!("no session matching '{target}'")),
        _ => Err(format!("ambiguous session id '{target}'")),
    }
}

/// Refresh the `session_write` entries snapshot from the active
/// session file: a trimmed `{ id, kind, ts, role? }` per entry in
/// file order. Run once per worker request (a between-turn cadence,
/// never per stream tick) so a granted plugin's
/// `kage.session.entries()` reflects the latest committed turn. A
/// missing file (no turn yet) clears the snapshot.
pub(crate) fn refresh_session_entries(
    plugin_runtime: Option<&Arc<PluginRuntime>>,
    session_path: Option<&Arc<Mutex<PathBuf>>>,
) {
    let Some(rt) = plugin_runtime else {
        return;
    };
    let Some(sp) = session_path else {
        return;
    };
    let path = sp.lock().expect("session path mutex poisoned").clone();
    let Ok(reader) = SessionReader::iter(&path) else {
        rt.set_session_entries(Vec::new());
        return;
    };
    let mut out = Vec::new();
    for item in reader {
        let Ok(entry) = item else {
            break;
        };
        let mut obj = serde_json::json!({
            "id": entry.id().to_string(),
            "kind": entry_kind(&entry),
            "ts": entry.ts().to_rfc3339(),
        });
        if let kage_session::SessionEntry::Message(m) = &entry
            && let Ok(role) = serde_json::to_value(m.message.role)
        {
            obj["role"] = role;
            // First text block, if any. Lets plugin labels show what
            // the message actually said instead of only ts + id (the
            // rewind picker is the main consumer).
            for block in &m.message.content {
                if let kage_core::Content::Text { text } = block
                    && !text.is_empty()
                {
                    obj["text"] = serde_json::Value::String(text.clone());
                    break;
                }
            }
        }
        out.push(obj);
    }
    rt.set_session_entries(out);
}

/// Stable short name for a session entry variant, used as the `kind`
/// field of the `session_write` entries snapshot. Exhaustive so a new
/// variant fails to compile here rather than silently going unnamed.
pub(crate) fn entry_kind(entry: &kage_session::SessionEntry) -> &'static str {
    use kage_session::SessionEntry as E;
    match entry {
        E::Header(_) => "header",
        E::Message(_) => "message",
        E::ThinkingLevelChange(_) => "thinking_level_change",
        E::ModelChange(_) => "model_change",
        E::Compaction(_) => "compaction",
        E::Label(_) => "label",
        E::Title(_) => "title",
        E::Custom(_) => "custom",
    }
}
