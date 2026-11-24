//! Session relocation is explicit and requires an offline confirmation.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kage_session::{EntryId, FORMAT_VERSION, Header, SessionId, SessionWriter};

struct Env {
    dir: tempfile::TempDir,
    path: PathBuf,
    header: Header,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data/kage/sessions");
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: chrono::Utc::now(),
            cwd: dir.path().to_path_buf(),
            model: "migration-test/model".into(),
            backend: None,
            system_prompt: "find-this-flat-session".into(),
            parent_session: None,
            parent_entry: None,
        };
        let path = root.join(format!("{}.jsonl", header.session));
        drop(SessionWriter::create(&path, header.clone()).unwrap());
        Self { dir, path, header }
    }

    fn run(&self, args: &[&str]) -> Output {
        let root = self.dir.path();
        Command::new(env!("CARGO_BIN_EXE_kage"))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .current_dir(root)
            .output()
            .unwrap()
    }

    fn root(&self) -> &Path {
        self.path.parent().unwrap()
    }
}

#[test]
fn normal_commands_discover_flat_history_without_relocation() {
    let env = Env::new();
    let before = std::fs::read(&env.path).unwrap();
    let listed = env.run(&["list"]);
    assert!(listed.status.success(), "{listed:?}");
    assert!(String::from_utf8_lossy(&listed.stdout).contains(&env.header.session.to_string()[..8]));
    let searched = env.run(&["search", "find-this-flat-session"]);
    assert!(searched.status.success(), "{searched:?}");
    assert!(
        String::from_utf8_lossy(&searched.stdout).contains("[header] model=migration-test/model")
    );
    let forked = env.run(&[
        "fork",
        &env.header.session.to_string(),
        "--at",
        &env.header.id.to_string(),
    ]);
    assert!(forked.status.success(), "{forked:?}");
    let resumed = env.run(&[
        "--standalone",
        "resume",
        &env.header.session.to_string(),
        "--print",
        "test",
    ]);
    assert!(
        !resumed.status.success(),
        "the fixture has no provider credentials"
    );
    assert!(!String::from_utf8_lossy(&resumed.stderr).contains("no session"));
    assert_eq!(std::fs::read(&env.path).unwrap(), before);
    assert!(!env.root().join(".layout.lock").exists());
    assert!(!env.root().join(".layout-skipped").exists());
}

#[test]
fn migration_requires_offline_confirmation_and_moves_only_when_requested() {
    let env = Env::new();
    let before = std::fs::read(&env.path).unwrap();
    let unconfirmed = env.run(&["migrate-sessions"]);
    assert_eq!(unconfirmed.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unconfirmed.stderr).contains("--offline"));
    assert_eq!(std::fs::read(&env.path).unwrap(), before);

    let migrated = env.run(&["migrate-sessions", "--offline"]);
    assert!(migrated.status.success(), "{migrated:?}");
    assert!(!env.path.exists());
    let destination = kage_session::find_by_prefix(env.root(), &env.header.session.to_string())
        .unwrap()
        .unwrap();
    assert_ne!(destination, env.path);
    assert_eq!(std::fs::read(destination).unwrap(), before);
    let repeated = env.run(&["migrate-sessions", "--offline"]);
    assert!(repeated.status.success(), "{repeated:?}");
}
