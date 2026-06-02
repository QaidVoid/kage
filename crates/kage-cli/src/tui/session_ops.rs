//! Fork/clone/new/export/resume/delete session operations.

#[allow(clippy::wildcard_imports)] // tui split: shares the parent module scope
use super::*;

/// Handle a plugin-initiated fork. Copies the current session up
/// through entry `at` (or the latest entry when `at` is empty) into a
/// fresh session file, pushes a toast with the new id, and returns the
/// new file's path. `RunRequest::ForkSession` ignores the path (a
/// fork-and-stay snapshot); `SwitchSession(PendingFork)` reseats onto
/// it. Returns `None` on any error, which is surfaced as a
/// `kage:error` block.
pub(crate) fn handle_plugin_fork(
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
    at: &str,
) -> Option<PathBuf> {
    let Some(sp) = session_path else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "fork: no active session to fork".to_owned(),
                false,
            );
        }
        return None;
    };
    let src_path = sp.lock().expect("session path mutex poisoned").clone();
    if !src_path.exists() {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "fork: current session has no committed entries yet".to_owned(),
                false,
            );
        }
        return None;
    }
    let entry = if at.is_empty() {
        match find_last_entry(&src_path) {
            Ok(Some(id)) => id,
            Ok(None) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom(
                        "kage:error",
                        "fork: current session has no entries to fork at".to_owned(),
                        false,
                    );
                }
                return None;
            }
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom("kage:error", format!("fork: {e}"), false);
                }
                return None;
            }
        }
    } else {
        match kage_session::resolve_entry_prefix(&src_path, at) {
            Ok(id) => id,
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom("kage:error", format!("fork: {e}"), false);
                }
                return None;
            }
        }
    };
    let Some(dir) = src_path.parent() else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "fork: session path has no parent directory".to_owned(),
                false,
            );
        }
        return None;
    };
    let new_session = SessionId::new();
    let dst = dir.join(format!("{new_session}.jsonl"));
    if let Err(e) = kage_session::fork(&src_path, &dst, new_session, entry) {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", format!("fork failed: {e}"), false);
        }
        return None;
    }
    let short: String = new_session.to_string().chars().take(8).collect();
    push_toast(toasts, Toast::info(format!("forked session: {short}")));
    Some(dst)
}

/// Handle [`RunRequest::CloneSession`]. Forks the active session at
/// its last entry into a fresh id, then reseats `session_path` onto
/// the copy so every subsequent turn appends there. The original file
/// is frozen as a snapshot. History, model, and usage need no
/// adjustment: the clone is byte-identical through the last entry, so
/// the in-memory context already matches it. Errors surface as
/// `kage:error` blocks; success raises a toast with the new id.
pub(crate) fn handle_clone(
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
) {
    let Some(sp) = session_path else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "clone: no active session to clone".to_owned(),
                false,
            );
        }
        return;
    };
    let src_path = sp.lock().expect("session path mutex poisoned").clone();
    if !src_path.exists() {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "clone: current session has no committed entries yet".to_owned(),
                false,
            );
        }
        return;
    }
    let entry = match find_last_entry(&src_path) {
        Ok(Some(id)) => id,
        Ok(None) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    "clone: current session has no entries to clone".to_owned(),
                    false,
                );
            }
            return;
        }
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("clone: {e}"), false);
            }
            return;
        }
    };
    let Some(dir) = src_path.parent() else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "clone: session path has no parent directory".to_owned(),
                false,
            );
        }
        return;
    };
    let new_session = SessionId::new();
    let dst = dir.join(format!("{new_session}.jsonl"));
    if let Err(e) = kage_session::fork(&src_path, &dst, new_session, entry) {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", format!("clone failed: {e}"), false);
        }
        return;
    }
    sp.lock()
        .expect("session path mutex poisoned")
        .clone_from(&dst);
    let short: String = new_session.to_string().chars().take(8).collect();
    push_toast(
        toasts,
        Toast::info(format!("cloned session: {short} (continuing in clone)")),
    );
}

/// Handle [`RunRequest::NewSession`]. Plans a fresh session file (its
/// creation deferred to the first prompt, exactly like startup),
/// clears the in-memory history and token budget, zeroes the usage
/// modeline, wipes the rendered buffer, and reseats `session_path` /
/// `session_header` onto the new file. The active model and system
/// prompt are preserved; the prior session file is left intact.
pub(crate) fn handle_new(
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    session_header: Option<&Arc<Mutex<Option<kage_session::Header>>>>,
    cx: &Arc<Mutex<AgentContext>>,
    active_qualified: &Arc<Mutex<String>>,
    session_usage: &SharedSessionUsage,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
) {
    let (Some(sp), Some(sh)) = (session_path, session_header) else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "new: session storage unavailable".to_owned(),
                false,
            );
        }
        return;
    };
    let qualified = active_qualified
        .lock()
        .expect("active model mutex poisoned")
        .clone();
    let system_prompt = cx
        .lock()
        .expect("agent context mutex poisoned")
        .system_prompt
        .clone();
    let (path, header) = match crate::plan_session(&qualified, &system_prompt) {
        Ok(pair) => pair,
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("new: {e}"), false);
            }
            return;
        }
    };
    let short: String = header.session.to_string().chars().take(8).collect();
    {
        let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
        cx_guard.history.clear();
        cx_guard.budget.used_input = 0;
        cx_guard.budget.used_output = 0;
        cx_guard.budget.used_cache_read = 0;
        cx_guard.budget.used_cache_write = 0;
        cx_guard.budget.current_context = 0;
    }
    sh.lock()
        .expect("session header mutex poisoned")
        .replace(header);
    sp.lock()
        .expect("session path mutex poisoned")
        .clone_from(&path);
    if let Ok(mut snap) = session_usage.lock() {
        snap.input_tokens = 0;
        snap.output_tokens = 0;
        snap.cache_read_tokens = 0;
        snap.cache_write_tokens = 0;
        snap.current_context = 0;
        snap.total_cost = 0.0;
    }
    if let Ok(mut buf) = buffer.lock() {
        buf.clear();
    }
    push_toast(toasts, Toast::info(format!("new session: {short}")));
}

/// Handle [`RunRequest::ExportSession`]. Replays the active session
/// file (the source of truth, not the rendered buffer) and writes a
/// Markdown transcript. `None` targets `<short-id>.md` in the working
/// directory. Errors surface as `kage:error` blocks; success raises a
/// toast with the written path.
pub(crate) fn handle_export(
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    dest: Option<PathBuf>,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
) {
    let Some(sp) = session_path else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", "export: no active session".to_owned(), false);
        }
        return;
    };
    let src = sp.lock().expect("session path mutex poisoned").clone();
    if !src.exists() {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "export: current session has no committed entries yet".to_owned(),
                false,
            );
        }
        return;
    }
    let replay = match kage_session::replay(&src) {
        Ok(r) => r,
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("export: {e}"), false);
            }
            return;
        }
    };
    let out = dest.unwrap_or_else(|| {
        let short: String = replay.header.session.to_string().chars().take(8).collect();
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(format!("{short}.md"))
    });
    let markdown = render_session_markdown(&replay);
    if let Err(e) = std::fs::write(&out, markdown) {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                format!("export: write {}: {e}", out.display()),
                false,
            );
        }
        return;
    }
    push_toast(
        toasts,
        Toast::info(format!("exported to {}", out.display())),
    );
}

/// Render a replayed session as a Markdown transcript. Plain text and
/// fenced code only, no HTML, so the file stays greppable and
/// ASCII-clean.
pub(crate) fn render_session_markdown(replay: &kage_session::ReplayResult) -> String {
    use std::fmt::Write as _;
    let short: String = replay.header.session.to_string().chars().take(8).collect();
    let mut md = String::new();
    let _ = writeln!(md, "# kage session {short}");
    let _ = writeln!(md);
    let _ = writeln!(md, "- model: `{}`", replay.model);
    let _ = writeln!(md, "- created: {}", replay.header.ts.to_rfc3339());
    let _ = writeln!(md, "- cwd: `{}`", replay.header.cwd.display());
    let _ = writeln!(md);
    for msg in &replay.history {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::ToolResult => "Tool",
            Role::System => "System",
        };
        let _ = writeln!(md, "## {role}");
        let _ = writeln!(md);
        for block in &msg.content {
            match block {
                Content::Text { text } => {
                    let _ = writeln!(md, "{text}");
                    let _ = writeln!(md);
                }
                Content::Thinking { text } => {
                    let _ = writeln!(md, "**thinking**");
                    let _ = writeln!(md);
                    for line in text.lines() {
                        let _ = writeln!(md, "> {line}");
                    }
                    let _ = writeln!(md);
                }
                Content::ToolCall { name, input, .. } => {
                    let pretty =
                        serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
                    let _ = writeln!(md, "**tool call: `{name}`**");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "```json");
                    let _ = writeln!(md, "{pretty}");
                    let _ = writeln!(md, "```");
                    let _ = writeln!(md);
                }
                Content::ToolResultBlock {
                    output, is_error, ..
                } => {
                    let label = if *is_error {
                        "tool result (error)"
                    } else {
                        "tool result"
                    };
                    let _ = writeln!(md, "**{label}**");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "```");
                    let _ = writeln!(md, "{output}");
                    let _ = writeln!(md, "```");
                    let _ = writeln!(md);
                }
                Content::Image { mime, .. } => {
                    let _ = writeln!(md, "_[image omitted: {mime}]_");
                    let _ = writeln!(md);
                }
                Content::Custom { kind, data } => {
                    let pretty =
                        serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
                    let _ = writeln!(md, "_[custom block: {kind}]_");
                    let _ = writeln!(md);
                    let _ = writeln!(md, "```json");
                    let _ = writeln!(md, "{pretty}");
                    let _ = writeln!(md, "```");
                    let _ = writeln!(md);
                }
            }
        }
    }
    md
}

/// Fork an arbitrary session file (the `:tree` browser's `f`) at its
/// last entry into a fresh session, leaving the runtime untouched.
pub(crate) fn handle_fork_file(
    path: &std::path::Path,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
) {
    if !path.exists() {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "fork: session file not found".to_owned(),
                false,
            );
        }
        return;
    }
    let entry = match find_last_entry(path) {
        Ok(Some(id)) => id,
        Ok(None) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    "fork: session has no entries to fork at".to_owned(),
                    false,
                );
            }
            return;
        }
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("fork: {e}"), false);
            }
            return;
        }
    };
    let Some(dir) = path.parent() else {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom(
                "kage:error",
                "fork: session path has no parent directory".to_owned(),
                false,
            );
        }
        return;
    };
    let new_session = SessionId::new();
    let dst = dir.join(format!("{new_session}.jsonl"));
    if let Err(e) = kage_session::fork(path, &dst, new_session, entry) {
        if let Ok(mut buf) = buffer.lock() {
            buf.push_custom("kage:error", format!("fork failed: {e}"), false);
        }
        return;
    }
    let short: String = new_session.to_string().chars().take(8).collect();
    push_toast(toasts, Toast::info(format!("forked session: {short}")));
}

/// Delete a session file (the `:tree` browser's `d`). Refuses to
/// delete the session that is currently active so the live writer is
/// never orphaned; the refusal is surfaced, not silent.
pub(crate) fn handle_delete_session(
    path: &std::path::Path,
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    buffer: &SharedBuffer,
    toasts: &SharedToasts,
) {
    if let Some(sp) = session_path {
        let active = sp.lock().expect("session path mutex poisoned").clone();
        if active.as_path() == path {
            push_toast(
                toasts,
                Toast::info("cannot delete the active session".to_owned()),
            );
            return;
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            let short: String = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.chars().take(8).collect())
                .unwrap_or_default();
            push_toast(toasts, Toast::info(format!("deleted session: {short}")));
        }
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", format!("delete failed: {e}"), false);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_resume(
    registry: &Arc<ProviderRegistry>,
    active_qualified: &Arc<Mutex<String>>,
    cx: &Arc<Mutex<AgentContext>>,
    buffer: &SharedBuffer,
    session_path: Option<&Arc<Mutex<PathBuf>>>,
    session_usage: &SharedSessionUsage,
    toasts: &SharedToasts,
    path: &std::path::Path,
) {
    let replay = match kage_session::replay(path) {
        Ok(r) => r,
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom(
                    "kage:error",
                    format!("resume {}: {e}", path.display()),
                    false,
                );
            }
            return;
        }
    };
    let active_now = active_qualified
        .lock()
        .expect("active model mutex poisoned")
        .clone();
    let (qualified_model, bare_model, fallback_note) = match registry.resolve(&replay.model) {
        Ok(r) => (replay.model.clone(), r.model.clone(), None),
        Err(_) => match registry.resolve(&active_now) {
            Ok(r) => (
                active_now.clone(),
                r.model.clone(),
                Some(format!(
                    "session model {} unavailable; using {} instead",
                    replay.model, active_now
                )),
            ),
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom(
                        "kage:error",
                        format!("resume: no resolvable model ({e})"),
                        false,
                    );
                }
                return;
            }
        },
    };
    let context_window;
    let resumed_level = replay
        .thinking_level
        .as_deref()
        .and_then(kage_loop::ThinkingLevel::parse);
    {
        let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
        cx_guard.history.clone_from(&replay.history);
        cx_guard.model = bare_model;
        if let Some(window) = crate::runtime_env::context_window_for(registry, &qualified_model) {
            cx_guard.context_window = window;
        }
        cx_guard.max_output_tokens =
            crate::runtime_env::max_output_tokens_for(registry, &qualified_model);
        cx_guard.thinking_level = resumed_level;
        cx_guard.budget.used_input = replay.usage_total.input;
        cx_guard.budget.used_output = replay.usage_total.output;
        cx_guard.budget.used_cache_read = replay.usage_total.cache_read;
        cx_guard.budget.used_cache_write = replay.usage_total.cache_write;
        cx_guard.budget.current_context = replay.usage_total.last_context;
        context_window = cx_guard.context_window;
    }
    if let Ok(mut snap) = session_usage.lock() {
        snap.model.clone_from(&qualified_model);
        snap.context_window = context_window;
        snap.input_tokens = replay.usage_total.input;
        snap.output_tokens = replay.usage_total.output;
        snap.cache_read_tokens = replay.usage_total.cache_read;
        snap.cache_write_tokens = replay.usage_total.cache_write;
        snap.current_context = replay.usage_total.last_context;
        snap.thinking_level = resumed_level;
    }
    active_qualified
        .lock()
        .expect("active model mutex poisoned")
        .clone_from(&qualified_model);
    if let Some(sp) = session_path {
        sp.lock()
            .expect("session path mutex poisoned")
            .clone_from(&path.to_path_buf());
    }
    let id = replay.header.session.to_string();
    let short: String = id.chars().take(8).collect();
    if let Ok(mut buf) = buffer.lock() {
        buf.clear();
        populate_from_history(&mut buf, &replay.history, &replay.tool_durations);
    }
    push_toast(
        toasts,
        Toast::info(format!("resumed session {short} on {qualified_model}")),
    );
    if let Some(note) = fallback_note {
        push_toast(toasts, Toast::info(note));
    }
}

/// Build the picker rows the App offers when the user hits `Ctrl+P`.
/// Iterates registered providers and pulls each one's catalog model
/// list; when the catalog has no entry for a provider (e.g. plugin-
/// registered providers), falls back to the live `Provider::models()`
/// list. The active model is marked with `*`.
pub(crate) fn available_model_items(
    registry: &ProviderRegistry,
    active: &str,
) -> Vec<kage_tui::PickItem> {
    let mut items: Vec<kage_tui::PickItem> = Vec::new();
    let mut provider_ids: Vec<&str> = registry.ids().collect();
    provider_ids.sort_unstable();
    for provider_id in provider_ids {
        let catalog_provider = kage_provider::catalog::provider(provider_id);
        let catalog_models = catalog_provider.map_or::<&[_], _>(&[], |p| p.models);
        if !catalog_models.is_empty() {
            let display_name = catalog_provider.map_or(provider_id, |p| p.name);
            for model in catalog_models {
                let value = format!("{provider_id}:{}", model.id);
                let badge = if value == active { '*' } else { ' ' };
                items.push(
                    kage_tui::PickItem::simple(value)
                        .with_label(model.name)
                        .with_badge(badge)
                        .with_group(display_name),
                );
            }
            continue;
        }
        let Some(provider) = registry.get(provider_id) else {
            continue;
        };
        let metadata = provider.metadata();
        let display_name = metadata.display_name.as_str();
        for model in provider.models() {
            let value = format!("{provider_id}:{}", model.id);
            let badge = if value == active { '*' } else { ' ' };
            items.push(
                kage_tui::PickItem::simple(value)
                    .with_label(&model.name)
                    .with_badge(badge)
                    .with_group(display_name),
            );
        }
    }
    items
}
