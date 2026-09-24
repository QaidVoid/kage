//! Session title, bridge, dialog, and listing helpers.

#[allow(clippy::wildcard_imports)] // tui split: shares the parent module scope
use super::*;

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
    commander: &Commander,
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
    drive_bridge(rt, &label, step, dialog_tx, commander)
}

/// Run a key mapping's Lua handler through the coroutine bridge,
/// same servicing path as a command (so the handler may open
/// `kage.ui.*` dialogs). A non-empty return value is surfaced as a
/// conversation block, just like a command's output.
pub(crate) fn run_bridged_keymap(
    rt: &PluginRuntime,
    id: u64,
    dialog_tx: &mpsc::Sender<PluginDialog>,
    commander: &Commander,
) -> Option<CommandOutput> {
    let label = "key mapping";
    let handler = match rt.keymap_handler(id) {
        Ok(handler) => handler,
        Err(e) => return Some(error_output(label, &e.to_string())),
    };
    let step = match rt.bridge_call(&handler, &[]) {
        Ok(step) => step,
        Err(e) => return Some(error_output(label, &e.to_string())),
    };
    drive_bridge(rt, label, step, dialog_tx, commander)
}

/// Drive a started bridge call to completion: service each suspend
/// through the App's dialog channel, resume/cancel, and on a terminal
/// `Done` map the value to a [`CommandOutput`]. Shared by the command
/// and key mapping paths.
pub(crate) fn drive_bridge(
    rt: &PluginRuntime,
    label: &str,
    mut step: BridgeStep,
    dialog_tx: &mpsc::Sender<PluginDialog>,
    commander: &Commander,
) -> Option<CommandOutput> {
    loop {
        match step {
            BridgeStep::Done(value) => return Some(CommandOutput::from_json(&value)),
            BridgeStep::Suspended(req) => {
                let resumed = match service_dialog(&req, dialog_tx, commander) {
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
    commander: &Commander,
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
                dialog_error(commander, format!("ui.select: {e}"));
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
                dialog_error(commander, format!("ui.confirm: {e}"));
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
                dialog_error(commander, format!("ui.input: {e}"));
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
                dialog_error(commander, format!("ui.editor: {e}"));
                return None;
            }
        },
        other => {
            dialog_error(commander, format!("unsupported plugin dialog: {other}"));
            return None;
        }
    };
    if dialog_tx.send(dialog).is_err() {
        return None;
    }
    reply_rx.recv().unwrap_or(None)
}

fn dialog_error(commander: &Commander, text: String) {
    commander.publish(kage_core::protocol::HostEvent::Notice {
        level: kage_core::protocol::NoticeLevel::Error,
        text,
        transient: false,
    });
}

/// Build a one-line error [`CommandOutput`] for a failed plugin
/// invocation. `label` reads like `command foo` or `key mapping`.
pub(crate) fn error_output(label: &str, msg: &str) -> CommandOutput {
    CommandOutput {
        text: format!("plugin {label}: {msg}"),
        is_error: true,
        structured: None,
    }
}

/// Build the picker rows for the `Ctrl+S` session picker. Listing
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
    current: Option<&std::path::Path>,
) -> Vec<kage_tui::SessionNode> {
    let Ok(summaries) = kage_session::list(dir) else {
        return Vec::new();
    };
    summaries
        .into_iter()
        .map(|s| {
            let is_current = current == Some(s.path.as_path());
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

/// Snapshot the plugin commands (regulars then overrides) the runtime
/// currently holds, translated into the App's [`PluginCommand`] shape.
/// Shared by startup seeding and the post-reload republish so both
/// paths stay in sync.
pub(crate) fn snapshot_plugin_commands(
    rt: &PluginRuntime,
) -> Vec<kage_tui::command::PluginCommand> {
    let mut listing = Vec::new();
    for cmd in rt.registered_commands() {
        listing.push(kage_tui::command::PluginCommand {
            name: cmd.name().to_owned(),
            aliases: cmd.aliases().to_vec(),
            is_override: false,
            description: cmd.description().to_owned(),
            args: cmd.args().iter().map(translate_plugin_arg).collect(),
        });
    }
    for cmd in rt.registered_command_overrides() {
        listing.push(kage_tui::command::PluginCommand {
            name: cmd.name().to_owned(),
            aliases: cmd.aliases().to_vec(),
            is_override: true,
            description: cmd.description().to_owned(),
            args: cmd.args().iter().map(translate_plugin_arg).collect(),
        });
    }
    listing
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
    session_path: Option<&std::path::Path>,
) {
    let (Some(rt), Some(path)) = (plugin_runtime, session_path) else {
        return;
    };
    let Ok(reader) = SessionReader::iter(path) else {
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

/// Point the TUI's block registry at `rt`'s block renderers, replacing any
/// earlier plugin registrations.
pub(crate) fn register_block_renderers(rt: &PluginRuntime) {
    use kage_tui::view::registry;
    registry::reset_to_builtins();
    for renderer in rt.registered_block_renderers() {
        let kind = renderer.kind().to_owned();
        let factory = Arc::new(kage_tui::view::plugin_block::PluginBlockFactory::new(
            renderer,
        ));
        match registry::builtin_kind_from_name(&kind) {
            Some(builtin) => registry::register_builtin(builtin, factory),
            None => registry::register_custom(kind, factory),
        }
    }
}
