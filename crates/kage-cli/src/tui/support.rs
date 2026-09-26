//! Session title, bridge, dialog, listing, and start notice helpers.

use super::*;

use chrono::{DateTime, Duration, Local, Utc};
use kage_core::protocol::NoticeLevel;

use crate::auth::AuthStore;
use crate::state::State;

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
        level: NoticeLevel::Error,
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
/// default); the in-picker `Ctrl+A` toggle re-lists with `all`. Agent
/// sessions are never shown.
pub(crate) fn list_session_choices(
    dir: &std::path::Path,
    workdir: &std::path::Path,
    all: bool,
) -> Vec<PickItem> {
    let Ok(mut summaries) = kage_session::list(dir) else {
        return Vec::new();
    };
    summaries.retain(|s| s.agent.is_none() && (all || s.cwd == workdir));
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
            let time = s
                .updated_at
                .with_timezone(&Local)
                .format("%H:%M")
                .to_string();
            PickItem::simple(s.path.to_string_lossy().into_owned())
                .with_label(format_session_label(&s))
                .with_group(day)
                .with_right(time)
        })
        .collect()
}

/// Build the `:tree` forest rows from the sessions directory, marking
/// whichever file the runtime is currently writing as the active one.
/// Agent sessions sit under their parent with an `agent: ` label.
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
                label: match s.agent {
                    Some(_) => format!("agent: {}", format_session_label(&s)),
                    None => format_session_label(&s),
                },
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
            |t| t.split_whitespace().collect::<Vec<_>>().join(" "),
        )
}

/// A short, human day label relative to now in local time: `Today` /
/// `Yesterday` for the last two days, otherwise `YYYY-MM-DD`. Drives
/// the at-a-glance grouping in the session picker.
pub(crate) fn relative_day(ts: chrono::DateTime<chrono::Utc>) -> String {
    let today = Local::now().date_naive();
    let day = ts.with_timezone(&Local).date_naive();
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
/// currently holds, translated into the App's
/// [`kage_tui::command::PluginCommand`] shape. Shared by startup seeding
/// and the post-reload republish so both paths stay in sync.
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

/// Build the picker rows the App offers when the user hits `ctrl+p`.
/// Iterates registered providers and lists the models each one
/// declares (custom and plugin providers); a provider that declares
/// none (the built-ins) falls back to its catalog model list. The
/// active model is marked with `*`, and each row lists the inputs the
/// model accepts when they are known.
pub(crate) fn available_model_items(
    registry: &ProviderRegistry,
    active: &str,
) -> Vec<kage_tui::PickItem> {
    let row = |value: String, label: &str, group: &str, input: kage_core::Inputs| {
        let badge = if value == active { '*' } else { ' ' };
        with_inputs(
            kage_tui::PickItem::simple(value)
                .with_label(label)
                .with_badge(badge)
                .with_group(group),
            input,
        )
    };
    let mut items: Vec<kage_tui::PickItem> = Vec::new();
    let mut provider_ids: Vec<&str> = registry.ids().collect();
    provider_ids.sort_unstable();
    for provider_id in provider_ids {
        let Some(provider) = registry.get(provider_id) else {
            continue;
        };
        let declared = provider.models();
        if !declared.is_empty() {
            let group = provider.metadata().display_name.as_str();
            for model in declared {
                let value = format!("{provider_id}:{}", model.id);
                items.push(row(value, &model.name, group, model.input));
            }
            continue;
        }
        let Some(catalog) = kage_provider::catalog::provider(provider_id) else {
            continue;
        };
        for model in catalog.models {
            let value = format!("{provider_id}:{}", model.id);
            items.push(row(value, model.name, catalog.name, model.input));
        }
    }
    items
}

/// `item` with `input` in its right column, unless the inputs are
/// unknown.
fn with_inputs(item: kage_tui::PickItem, input: kage_core::Inputs) -> kage_tui::PickItem {
    if input.is_empty() {
        item
    } else {
        item.with_right(input.label())
    }
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

/// How far ahead an expiring OAuth credential is reported at startup.
const OAUTH_EXPIRY_WARNING: Duration = Duration::days(3);

/// Notices for the start screen: the version-updated line, a configured
/// default model that does not resolve, OAuth credentials close to
/// expiry, auth failures recorded on earlier runs, and the project
/// settings ignored because the project is not trusted. `model` is the
/// model kage picked on its own, `None` when `--model` chose it, which
/// skips the default-model notice. Records the running version as
/// seen.
pub(crate) fn start_notices(
    registry: &ProviderRegistry,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<(NoticeLevel, String)> {
    let mut notices = Vec::new();
    let current = env!("CARGO_PKG_VERSION");
    match crate::state::record_version_seen(current) {
        Ok(Some(prev)) => notices.push((
            NoticeLevel::Info,
            format!("kage updated: {prev} -> {current}"),
        )),
        Ok(None) => {}
        Err(err) => notices.push((NoticeLevel::Error, format!("state: {err}"))),
    }
    if let Some(using) = model
        && let Some(configured) = crate::configured_default_model()
        && let Some(notice) = default_model_notice(registry, &configured, using)
    {
        notices.push((NoticeLevel::Warning, notice));
    }
    let auth = AuthStore::load().unwrap_or_else(|_| AuthStore::empty());
    notices.extend(
        credential_notices(&State::load(), &auth, now)
            .into_iter()
            .map(|text| (NoticeLevel::Warning, text)),
    );
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(summary) = kage_core::trust::untrusted_project(&workdir) {
        notices.push((NoticeLevel::Warning, untrusted_notice(&summary)));
    }
    notices
}

/// Notice naming the project settings and agents this run ignores
/// because the project is not trusted, and how to allow them.
fn untrusted_notice(summary: &kage_core::trust::TrustSummary) -> String {
    let mut ignored: Vec<String> = summary
        .keys
        .iter()
        .filter(|key| **key != "agents" && **key != "skills")
        .map(|key| (*key).to_owned())
        .collect();
    if !summary.agents.is_empty() {
        ignored.push(format!("agents ({})", summary.agents.join(", ")));
    }
    if !summary.skills.is_empty() {
        ignored.push(format!("skills ({})", summary.skills.join(", ")));
    }
    format!(
        "project settings ignored because the project is not trusted: {}. \
         Run `kage trust` here and restart to use them.",
        ignored.join(", ")
    )
}

/// Notice for a `configured` default model that does not resolve,
/// naming `using` as the model picked instead.
fn default_model_notice(
    registry: &ProviderRegistry,
    configured: &str,
    using: &str,
) -> Option<String> {
    if registry.resolve(configured).is_ok() {
        return None;
    }
    let provider = configured.split_once(':').map_or(configured, |(p, _)| p);
    Some(format!(
        "default_model {configured} is unavailable (no credentials for {provider}). \
         Using {using}. Run /login {provider} to connect it."
    ))
}

/// Warnings for OAuth credentials in `auth` expiring within
/// [`OAUTH_EXPIRY_WARNING`] and for the auth failures `state` recorded.
fn credential_notices(state: &State, auth: &AuthStore, now: DateTime<Utc>) -> Vec<String> {
    let expiring = auth
        .oauth_expiring(OAUTH_EXPIRY_WARNING, now)
        .map(|(provider, at)| {
            let when = match (at - now).num_days() {
                _ if at <= now => "has expired".to_owned(),
                0 => "expires within a day".to_owned(),
                1 => "expires in 1 day".to_owned(),
                days => format!("expires in {days} days"),
            };
            format!("the {provider} login {when}. Run /login {provider}.")
        });
    let failed = state.auth_failures.iter().map(|(provider, detail)| {
        format!(
            "{provider} rejected the credentials on the last run ({}). Run /login {provider}.",
            detail.trim_end_matches('.')
        )
    });
    expiring.chain(failed).collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kage_core::protocol::RunOutcome;
    use kage_core::{LoopError, SessionId};
    use kage_provider::testing::MockProvider;

    use super::*;
    use crate::auth::OAuthCredential;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn oauth_expiring_in(days: i64) -> OAuthCredential {
        OAuthCredential {
            access_token: "fake-access".into(),
            expires_at: Some(now() + Duration::days(days)),
            ..OAuthCredential::default()
        }
    }

    #[test]
    fn recorded_auth_failure_names_the_login_fix() {
        let mut state = State::empty();
        state.note_run(
            "zai-coding-plan:glm-4.6",
            &RunOutcome::Failed {
                error: LoopError::Auth {
                    message: "status 401".into(),
                },
            },
        );
        let notices = credential_notices(&state, &AuthStore::empty(), now());
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("status 401"), "{notices:?}");
        assert!(
            notices[0].contains("Run /login zai-coding-plan."),
            "{notices:?}"
        );
    }

    #[test]
    fn completed_run_clears_the_failure_notice() {
        let mut state = State::empty();
        let failed = RunOutcome::Failed {
            error: LoopError::Auth {
                message: "bad key".into(),
            },
        };
        state.note_run("zai:glm-4.6", &failed);
        state.note_run("zai:glm-4.6", &RunOutcome::Completed);
        assert!(credential_notices(&state, &AuthStore::empty(), now()).is_empty());
    }

    #[test]
    fn oauth_expiring_soon_warns_and_later_does_not() {
        let mut auth = AuthStore::empty();
        auth.set_oauth("anthropic", oauth_expiring_in(1));
        auth.set_oauth("openai", oauth_expiring_in(10));
        let notices = credential_notices(&State::empty(), &auth, now());
        assert_eq!(
            notices,
            ["the anthropic login expires in 1 day. Run /login anthropic."]
        );
    }

    #[test]
    fn expired_oauth_warns_and_api_keys_do_not() {
        let mut auth = AuthStore::empty();
        auth.set_oauth("anthropic", oauth_expiring_in(-1));
        auth.set_api_key("zai", "fake-key");
        let notices = credential_notices(&State::empty(), &auth, now());
        assert_eq!(
            notices,
            ["the anthropic login has expired. Run /login anthropic."]
        );
    }

    /// Write a session in `dir` titled `title`, spawned by `parent` as an
    /// agent when given.
    fn write_session(dir: &Path, title: &str, parent: Option<SessionId>) -> SessionId {
        use kage_session::{Custom, EntryId, FORMAT_VERSION, Header, SessionEntry, SessionTitle};

        let id = SessionId::new();
        let header = Header {
            version: FORMAT_VERSION,
            session: id,
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: dir.to_path_buf(),
            model: "mock:m".into(),
            system_prompt: String::new(),
            parent_session: parent,
            parent_entry: None,
        };
        let mut writer =
            kage_session::SessionWriter::create(dir.join(format!("{id}.jsonl")), header).unwrap();
        if parent.is_some() {
            writer
                .append(&SessionEntry::Custom(Custom {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    kind: kage_session::list::AGENT_ENTRY_KIND.into(),
                    data: serde_json::json!({ "agent": "explore" }),
                }))
                .unwrap();
        }
        writer
            .append(&SessionEntry::Title(SessionTitle {
                id: EntryId::new(),
                ts: Utc::now(),
                title: title.into(),
            }))
            .unwrap();
        id
    }

    #[test]
    fn picker_hides_agent_sessions_and_the_tree_labels_them() {
        let dir = tempfile::tempdir().unwrap();
        let main = write_session(dir.path(), "main work", None);
        write_session(dir.path(), "map exports", Some(main));

        for all in [false, true] {
            let labels: Vec<String> = list_session_choices(dir.path(), dir.path(), all)
                .into_iter()
                .map(|item| item.label)
                .collect();
            assert_eq!(labels, ["main work"]);
        }
        let mut nodes: Vec<(String, Option<String>)> = list_session_nodes(dir.path(), None)
            .into_iter()
            .map(|node| (node.label, node.parent))
            .collect();
        nodes.sort();
        assert_eq!(
            nodes,
            [
                ("agent: map exports".to_owned(), Some(main.to_string())),
                ("main work".to_owned(), None),
            ]
        );
    }

    #[test]
    fn untrusted_notice_names_what_is_ignored_and_the_fix() {
        let summary = kage_core::trust::TrustSummary {
            path: PathBuf::from("/p/.kage"),
            keys: vec!["mcp", "permissions", "agents", "skills"],
            items: Vec::new(),
            agents: vec!["reviewer".to_owned()],
            skills: vec!["helper".to_owned()],
        };
        assert_eq!(
            untrusted_notice(&summary),
            "project settings ignored because the project is not trusted: mcp, permissions, \
             agents (reviewer), skills (helper). Run `kage trust` here and restart to use them."
        );
    }

    #[test]
    fn model_picker_badges_the_active_row_only() {
        #[derive(Debug)]
        struct Stub {
            meta: kage_provider::ProviderMetadata,
            models: Vec<&'static str>,
        }

        impl kage_provider::Provider for Stub {
            fn metadata(&self) -> &kage_provider::ProviderMetadata {
                &self.meta
            }

            fn stream(
                &self,
                _req: kage_provider::StreamRequest,
                _cancel: &kage_core::CancelFlag,
            ) -> Result<kage_provider::EventStream, kage_provider::ProviderError> {
                Ok(Box::new(std::iter::empty()))
            }

            fn models(&self) -> Vec<kage_provider::ProviderModel> {
                self.models
                    .iter()
                    .map(|m| kage_provider::ProviderModel {
                        id: (*m).to_owned(),
                        name: (*m).to_owned(),
                        ..kage_provider::ProviderModel::default()
                    })
                    .collect()
            }
        }

        let registry = ProviderRegistry::new().with(std::sync::Arc::new(Stub {
            meta: kage_provider::ProviderMetadata {
                id: "p".to_owned(),
                display_name: "p".to_owned(),
                supports_caching: false,
                supports_thinking: false,
                supports_tool_use: true,
            },
            models: vec!["a", "b"],
        }));
        let items = available_model_items(&registry, "p:b");
        assert_eq!(items.len(), 2);
        let starred: Vec<_> = items.iter().filter(|i| i.badge == Some('*')).collect();
        assert_eq!(starred.len(), 1);
        assert_eq!(starred[0].value, "p:b");
        // A stale active id badges nothing instead of the default row.
        let stale = available_model_items(&registry, "p:gone");
        assert!(stale.iter().all(|i| i.badge != Some('*')));
    }

    #[test]
    fn default_model_notice_only_when_configured_model_does_not_resolve() {
        let registry = ProviderRegistry::new().with(Arc::new(MockProvider::replaying(Vec::new())));
        assert!(default_model_notice(&registry, "mock:m", "mock:m").is_none());
        let notice = default_model_notice(&registry, "anthropic:claude-sonnet-4-6", "mock:m")
            .expect("unresolved default warns");
        assert!(notice.contains("anthropic:claude-sonnet-4-6"), "{notice}");
        assert!(notice.contains("Using mock:m."), "{notice}");
        assert!(notice.contains("Run /login anthropic"), "{notice}");
    }
}
