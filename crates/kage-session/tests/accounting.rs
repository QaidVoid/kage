//! Saved attempt costs and billable ancestry across replay and forks.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use kage_core::{Content, CostStatus, Message, Role, TokenUsage};
use kage_session::{
    BranchEntry, Compaction, Custom, EntryId, FORMAT_VERSION, Header, MessageEntry, ModelChange,
    RejectedUsage, SessionEntry, SessionId, SessionReader, SessionTree, SessionWriter, UsageCost,
    fork, fork_as, replay, replay_tree,
};
use tempfile::tempdir;

fn assert_amount(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
}

fn header(model: &str) -> Header {
    Header {
        version: FORMAT_VERSION,
        session: SessionId::new(),
        id: EntryId::new(),
        ts: Utc::now(),
        cwd: "/work".into(),
        model: model.into(),
        system_prompt: String::new(),
        backend: None,
        parent_session: None,
        parent_entry: None,
    }
}

fn usage(input: u64) -> TokenUsage {
    TokenUsage {
        input,
        output: input * 2,
        cache_read: input * 3,
        cache_write: input * 4,
    }
}

fn message(role: Role, usage: Option<TokenUsage>) -> SessionEntry {
    SessionEntry::Message(MessageEntry {
        id: EntryId::new(),
        ts: Utc::now(),
        message: Arc::new(Message::new(
            role,
            vec![Content::Text {
                text: "response".into(),
            }],
            None,
        )),
        usage,
    })
}

fn accepted(input: u64) -> SessionEntry {
    message(Role::Assistant, Some(usage(input)))
}

fn annotation(target: &SessionEntry, amount: Option<f64>) -> SessionEntry {
    UsageCost {
        entry: target.id(),
        model: "saved/model".into(),
        amount,
    }
    .into_entry(EntryId::new(), Utc::now())
    .unwrap()
}

fn rejected(input: u64) -> SessionEntry {
    RejectedUsage {
        usage: usage(input),
    }
    .into_entry(EntryId::new(), Utc::now())
}

fn switch(model: &str) -> SessionEntry {
    SessionEntry::ModelChange(ModelChange {
        id: EntryId::new(),
        ts: Utc::now(),
        model: model.into(),
    })
}

fn marker(parent: SessionId) -> SessionEntry {
    SessionEntry::Custom(Custom {
        id: EntryId::new(),
        ts: Utc::now(),
        kind: "kage:agent".into(),
        data: serde_json::json!({"parent": parent}),
    })
}

fn write(path: &Path, header: Header, entries: &[SessionEntry]) {
    let mut writer = SessionWriter::create(path, header).unwrap();
    writer.append_many(entries).unwrap();
}

#[test]
fn recorded_unknown_free_and_legacy_costs_remain_distinct() {
    let dir = tempdir().unwrap();
    for (name, amount, expected_status) in [
        ("paid", Some(0.25), CostStatus::Recorded),
        ("free", Some(0.0), CostStatus::Recorded),
        ("unknown", None, CostStatus::Incomplete),
    ] {
        let path = dir.path().join(format!("{name}.jsonl"));
        let target = accepted(2);
        write(
            &path,
            header("anthropic:old"),
            &[annotation(&target, amount), target, switch("openai/new")],
        );
        let result = replay(&path).unwrap();
        assert_amount(result.accounting.cost, amount.unwrap_or_default());
        assert_eq!(result.accounting.cost_status, expected_status);
        assert_eq!(result.accounting.usage, usage(2));
        assert!(result.accounting.legacy_usage.is_empty());
        let (writer, reopened) = SessionWriter::open_native_replay(&path).unwrap().unwrap();
        assert_eq!(reopened.accounting, result.accounting);
        drop(writer);
    }
    let path = dir.path().join("legacy.jsonl");
    write(&path, header("anthropic:old"), &[accepted(2)]);
    let accounting = replay(&path).unwrap().accounting;
    assert_amount(accounting.cost, 0.0);
    assert_eq!(accounting.cost_status, CostStatus::Estimated);
    assert_eq!(accounting.legacy_usage["anthropic/old"], usage(2));
}

#[test]
fn legacy_costs_follow_historical_models_and_unknown_snapshots_never_enter_them() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("models.jsonl");
    let unknown = rejected(8);
    write(
        &path,
        header("anthropic:old"),
        &[
            accepted(1),
            switch("openai:new"),
            rejected(2),
            annotation(&unknown, None),
            unknown,
            switch("anthropic/old"),
            accepted(4),
        ],
    );
    let result = replay(&path).unwrap();
    assert_eq!(result.accounting.usage, usage(15));
    assert_eq!(result.accounting.legacy_usage.len(), 2);
    assert_eq!(result.accounting.legacy_usage["anthropic/old"], usage(5));
    assert_eq!(result.accounting.legacy_usage["openai/new"], usage(2));
    assert_eq!(result.accounting.cost_status, CostStatus::Incomplete);
}

#[test]
fn missing_assistant_usage_is_incomplete_even_with_a_saved_amount() {
    let dir = tempdir().unwrap();
    for annotated in [false, true] {
        let path = dir.path().join(format!("missing-{annotated}.jsonl"));
        let target = message(Role::Assistant, None);
        let mut entries = vec![message(Role::User, None)];
        if annotated {
            entries.push(annotation(&target, Some(0.5)));
        }
        entries.push(target);
        write(&path, header("m"), &entries);
        let result = replay(&path).unwrap();
        assert_eq!(result.accounting.cost_status, CostStatus::Incomplete);
        assert_amount(result.accounting.cost, if annotated { 0.5 } else { 0.0 });
        assert_eq!(result.accounting.usage, TokenUsage::default());
        assert!(result.accounting.legacy_usage.is_empty());
    }
}

#[test]
fn orphan_mismatched_and_nonadjacent_annotations_never_charge() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("orphans.jsonl");
    let target = accepted(1);
    let other = accepted(2);
    let orphan = accepted(4);
    write(
        &path,
        header("m"),
        &[
            annotation(&target, Some(100.0)),
            switch("new"),
            target.clone(),
            annotation(&other, Some(200.0)),
            annotation(&target, Some(300.0)),
            other,
            annotation(&orphan, Some(400.0)),
        ],
    );
    let result = replay(&path).unwrap();
    assert_amount(result.accounting.cost, 0.0);
    assert_eq!(result.accounting.usage, usage(3));
    assert_eq!(result.accounting.legacy_usage["new"], usage(3));
    assert_eq!(result.accounting.cost_status, CostStatus::Estimated);
}

#[test]
fn accepted_rejected_and_compaction_costs_survive_reopen_and_copy() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let first = accepted(1);
    let failed = rejected(2);
    let last = accepted(4);
    write(
        &path,
        header("m"),
        &[
            annotation(&first, Some(0.125)),
            first,
            SessionEntry::Compaction(Compaction {
                id: EntryId::new(),
                ts: Utc::now(),
                kept: 0,
                summarized: 1,
                summary: "summary".into(),
                before: Some(10),
            }),
            annotation(&failed, Some(0.25)),
            failed,
            annotation(&last, Some(0.5)),
            last,
        ],
    );
    let result = replay(&path).unwrap();
    assert_eq!(result.accounting.usage, usage(7));
    assert_amount(result.accounting.cost, 0.875);
    assert_eq!(result.accounting.cost_status, CostStatus::Recorded);
    assert!(result.accounting.legacy_usage.is_empty());
    assert_eq!(result.usage_total.input, 7);
    assert_eq!(result.usage_total.last_context, 40);
    assert_eq!(result.history.len(), 2);
    assert_eq!(result.timeline.len(), 3);
    let kage_core::protocol::TimelineItem::Compaction(card) = &result.timeline[1] else {
        panic!("missing card");
    };
    assert_eq!(card.after, Some(40));
    let copy = dir.path().join("copy.jsonl");
    std::fs::copy(&path, &copy).unwrap();
    assert_eq!(replay(&copy).unwrap().accounting, result.accounting);
    let (_, reopened) = SessionWriter::open_native_replay(&path).unwrap().unwrap();
    assert_eq!(reopened.accounting, result.accounting);
}

#[test]
fn branch_selection_and_forks_preserve_only_selected_charges() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let first = accepted(1);
    let discarded = rejected(100);
    let last = rejected(2);
    let cut = annotation(&last, Some(0.5));
    write(
        &path,
        header("m"),
        &[
            annotation(&first, Some(0.125)),
            first.clone(),
            annotation(&discarded, Some(100.0)),
            discarded.clone(),
            SessionEntry::Branch(BranchEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                at: first.id(),
            }),
            cut.clone(),
            last.clone(),
        ],
    );
    let result = replay(&path).unwrap();
    assert_amount(result.accounting.cost, 0.625);
    assert_eq!(result.accounting.usage, usage(3));
    assert_eq!(
        replay_tree(&path, SessionTree::read(&path).unwrap())
            .unwrap()
            .accounting,
        result.accounting
    );
    let forked = dir.path().join("forked.jsonl");
    fork(&path, &forked, SessionId::new(), last.id()).unwrap();
    assert_eq!(replay(&forked).unwrap().accounting, result.accounting);
    let before_target = dir.path().join("annotation-cut.jsonl");
    fork(&path, &before_target, SessionId::new(), cut.id()).unwrap();
    let cut_result = replay(&before_target).unwrap();
    assert_amount(cut_result.accounting.cost, 0.125);
    assert_eq!(cut_result.accounting.usage, usage(1));
    assert_eq!(cut_result.accounting.cost_status, CostStatus::Recorded);
    let alternate = dir.path().join("alternate.jsonl");
    fork(&path, &alternate, SessionId::new(), discarded.id()).unwrap();
    let alternate = replay(&alternate).unwrap();
    assert_amount(alternate.accounting.cost, 100.125);
    assert_eq!(alternate.accounting.usage, usage(101));
}

#[test]
fn forked_agent_resets_only_own_accounting_and_uses_its_header_model() {
    let dir = tempdir().unwrap();
    let parent_path = dir.path().join("parent.jsonl");
    let parent = header("parent:initial");
    let ancestor = SessionId::new();
    let prefix = accepted(10);
    let end = switch("parent:switched");
    write(
        &parent_path,
        parent.clone(),
        &[
            marker(ancestor),
            annotation(&prefix, None),
            prefix,
            message(Role::Assistant, None),
            end.clone(),
        ],
    );
    let agent_path = dir.path().join("agent.jsonl");
    let mut agent = header("agent:own");
    agent.parent_session = Some(parent.session);
    agent.parent_entry = Some(end.id());
    fork_as(&parent_path, &agent_path, agent, end.id()).unwrap();
    let mut writer = SessionWriter::open(&agent_path).unwrap();
    let known = accepted(2);
    writer
        .append_many(&[
            marker(parent.session),
            accepted(1),
            annotation(&known, Some(0.25)),
            known,
            switch("agent:next"),
            rejected(4),
        ])
        .unwrap();
    drop(writer);
    let result = replay(&agent_path).unwrap();
    assert_eq!(result.usage_total.input, 17);
    assert_eq!(result.usage_total.last_context, 20);
    assert_eq!(result.accounting.usage, usage(7));
    assert_amount(result.accounting.cost, 0.25);
    assert_eq!(result.accounting.cost_status, CostStatus::Estimated);
    assert_eq!(result.accounting.legacy_usage.len(), 2);
    assert_eq!(result.accounting.legacy_usage["agent/own"], usage(1));
    assert_eq!(result.accounting.legacy_usage["agent/next"], usage(4));
    assert_eq!(result.history.len(), 4);
    assert_eq!(result.timeline.len(), 4);
    assert!(kage_session::is_agent_session(&agent_path));
}

#[test]
fn ordinary_fork_of_agent_keeps_full_selected_ancestry_accounting() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("agent.jsonl");
    let parent = SessionId::new();
    let mut agent = header("m");
    agent.parent_session = Some(parent);
    let prefix = accepted(10);
    let own = rejected(1);
    write(
        &path,
        agent,
        &[
            annotation(&prefix, Some(1.0)),
            prefix,
            marker(parent),
            annotation(&own, Some(0.25)),
            own.clone(),
        ],
    );
    assert_amount(replay(&path).unwrap().accounting.cost, 0.25);
    let forked = dir.path().join("main-fork.jsonl");
    fork(&path, &forked, SessionId::new(), own.id()).unwrap();
    let result = replay(&forked).unwrap();
    assert!(!kage_session::is_agent_session(&forked));
    assert_amount(result.accounting.cost, 1.25);
    assert_eq!(result.accounting.usage, usage(11));
    assert_eq!(result.accounting.cost_status, CostStatus::Recorded);
}

#[test]
fn empty_batches_preserve_files_and_ordered_batches_preserve_message_schema() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("batch.jsonl");
    let mut writer = SessionWriter::create(&path, header("m")).unwrap();
    let before = std::fs::read(&path).unwrap();
    writer.append_many(&[]).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let target = accepted(1);
    let entries = [annotation(&target, Some(0.125)), target];
    writer.append_many(&entries).unwrap();
    let stored = SessionReader::iter(&path)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(&stored[1..], &entries);
    let SessionEntry::Message(message) = &stored[2] else {
        panic!("not message");
    };
    let fields = serde_json::to_value(message).unwrap();
    assert_eq!(fields.as_object().unwrap().len(), 4);
    assert!(fields.get("cost").is_none());
    assert_amount(replay(&path).unwrap().accounting.cost, 0.125);
}

#[test]
fn user_messages_and_unmatched_annotations_do_not_create_charges() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("user.jsonl");
    let user = message(Role::User, None);
    write(&path, header("m"), &[annotation(&user, Some(10.0)), user]);
    let result = replay(&path).unwrap();
    assert_eq!(result.accounting, kage_session::ReplayAccounting::default());
}
