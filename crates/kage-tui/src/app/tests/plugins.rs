//! Plugin commands, hot reload snapshots and the plugin session list.

use super::*;

#[test]
fn plugin_command_alias_resolves_to_canonical_invoke() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_plugin_commands(vec![PluginCommand {
        name: "git-status".into(),
        aliases: vec!["gst".into(), "gs".into()],
        is_override: false,
        description: "show git status".into(),
        args: Vec::new(),
    }]);
    let registry = app.command_registry();
    let res = app.run_command_validated("gst --short", &registry);
    assert!(matches!(res, CommandResult::Done(None)));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::InvokePluginCommand { name, args } => {
            assert_eq!(name, "git-status", "alias mapped to canonical");
            assert_eq!(args, "--short");
        }
        other => panic!("expected InvokePluginCommand, got {other:?}"),
    }
}

#[test]
fn plugin_command_alias_shadowing_builtin_is_dropped() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_plugin_commands(vec![PluginCommand {
        name: "mycmd".into(),
        aliases: vec!["help".into()],
        is_override: false,
        description: "tries to shadow :help via alias".into(),
        args: Vec::new(),
    }]);
    assert!(
        app.plugin_commands.is_empty(),
        "a command whose alias collides with a builtin is rejected whole"
    );
}

#[test]
fn set_plugin_commands_reuses_leaked_specs_on_unchanged_reload() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let cmd = || PluginCommand {
        name: "greet".into(),
        aliases: vec!["hi".into()],
        is_override: false,
        description: "say hello".into(),
        args: vec![crate::command::OwnedArgSpec::Text {
            name: "who".into(),
            optional: false,
            hint: "<who>".into(),
        }],
    };
    app.set_plugin_commands(vec![cmd()]);
    let first = app.plugin_command_specs[0];
    assert_eq!(app.plugin_commands_leaked.len(), 1);

    // Re-registering an identical set (the hot-reload case) reuses
    // the leaked spec instead of leaking a second copy.
    app.set_plugin_commands(vec![cmd()]);
    assert!(
        std::ptr::eq(app.plugin_command_specs[0], first),
        "equal reload reuses the leaked spec"
    );
    assert_eq!(app.plugin_commands_leaked.len(), 1);

    // A changed command leaks one fresh spec, which the next
    // unchanged reload then reuses.
    let mut changed = cmd();
    changed.description = "say hello loudly".into();
    app.set_plugin_commands(vec![changed.clone()]);
    assert_eq!(
        app.plugin_commands_leaked.len(),
        2,
        "changed command leaks a fresh spec"
    );
    assert!(!std::ptr::eq(app.plugin_command_specs[0], first));
    app.set_plugin_commands(vec![changed]);
    assert_eq!(
        app.plugin_commands_leaked.len(),
        2,
        "changed reload reuses too"
    );
}

#[test]
fn drain_plugin_refresh_reseeds_commands_and_widgets() {
    let buffer = shared_buffer();
    let (tx, _rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    let (rtx, rrx) = mpsc::channel();
    app.set_plugin_refresh(rrx);

    app.set_plugin_commands(vec![PluginCommand {
        name: "old".into(),
        aliases: Vec::new(),
        is_override: false,
        description: "pre-reload".into(),
        args: Vec::new(),
    }]);
    let pre = app.plugin_command_specs[0];

    rtx.send(PluginRefresh {
        commands: vec![PluginCommand {
            name: "zznew".into(),
            aliases: vec!["zzn".into()],
            is_override: false,
            description: "post-reload".into(),
            args: Vec::new(),
        }],
        widgets: Vec::new(),
        autocomplete: Vec::new(),
        models: Vec::new(),
    })
    .unwrap();
    assert!(app.drain_plugin_refresh(), "a queued snapshot applies");
    assert!(
        !std::ptr::eq(app.plugin_command_specs[0], pre),
        "commands re-seeded from the snapshot"
    );
    assert_eq!(app.plugin_commands.len(), 1);
    assert_eq!(app.plugin_commands[0].0, "zznew");
    assert!(
        app.plugin_texts_dirty,
        "widget reseed marks the text cache dirty"
    );
    assert!(!app.drain_plugin_refresh(), "drained channel is a no-op");
}

#[test]
fn override_command_shadows_builtin_and_dispatches_first() {
    let buffer = shared_buffer();
    let (tx, rx) = mpsc::channel();
    let mut app = app_with_defaults(buffer, tx);
    app.set_plugin_commands(vec![PluginCommand {
        name: "help".into(),
        aliases: Vec::new(),
        is_override: true,
        description: "my help".into(),
        args: Vec::new(),
    }]);
    assert_eq!(
        app.plugin_commands.len(),
        1,
        "override kept despite builtin"
    );
    let registry = app.command_registry();
    let res = app.run_command_validated("help", &registry);
    assert!(matches!(res, CommandResult::Done(None)));
    match rx.recv_timeout(Duration::from_millis(100)).unwrap() {
        RunRequest::InvokePluginCommand { name, .. } => {
            assert_eq!(name, "help", "override won over builtin :help");
        }
        other => panic!("expected plugin invoke, got {other:?}"),
    }
}

#[test]
fn the_plugin_session_list_refreshes_only_when_sessions_change() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (mut app, _rx, events) = app_with_events();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    app.set_session_lister(Box::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        vec![PickItem::simple("s1")]
    }));
    let list = kage_plugin::sessions::shared_session_list();
    app.set_plugin_session_list(list.clone());

    app.refresh_plugin_session_list_if_stale();
    app.refresh_plugin_session_list_if_stale();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(lock(&list).len(), 1);

    feed(&mut app, &events, vec![bash_start("c1")]);
    app.refresh_plugin_session_list_if_stale();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    feed(
        &mut app,
        &events,
        vec![
            kage_core::protocol::HostEvent::RunEnded {
                outcome: kage_core::protocol::RunOutcome::Completed,
            }
            .into(),
        ],
    );
    app.refresh_plugin_session_list_if_stale();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
