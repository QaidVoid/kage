//! Regression coverage for print-mode external session protection.

use std::process::Command;

use kage_session::{
    EntryId, ExternalAcpSession, FORMAT_VERSION, Header, SessionBackend, SessionEntry,
};

#[test]
fn print_resume_rejects_external_session_without_disclosing_identity_or_modifying_it() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("data/kage/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let session = kage_core::SessionId::new();
    let path = dir.join(format!("{session}.jsonl"));
    let header = Header {
        version: FORMAT_VERSION,
        session,
        id: EntryId::new(),
        ts: chrono::Utc::now(),
        cwd: temp.path().to_path_buf(),
        model: "private-model".into(),
        system_prompt: String::new(),
        backend: Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "private-agent".into(),
            protocol_version: 1,
            upstream_session_id: "private-upstream".into(),
        })),
        parent_session: None,
        parent_entry: None,
    };
    let contents = format!(
        "{}\n",
        serde_json::to_string(&SessionEntry::Header(header)).unwrap()
    );
    std::fs::write(&path, &contents).unwrap();
    for extra in [vec![], vec!["--json", "--model", "unknown/model"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_kage"))
            .env_clear()
            .env("HOME", temp.path())
            .env("XDG_DATA_HOME", temp.path().join("data"))
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .args(["resume", &session.to_string(), "--print", "hello"])
            .args(extra)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stderr.contains("external sessions cannot be resumed in print mode"),
            "{stderr}"
        );
        for secret in [
            "private-agent",
            "private-upstream",
            "private-model",
            path.to_str().unwrap(),
            &session.to_string(),
        ] {
            assert!(!stderr.contains(secret), "{stderr}");
            assert!(!stdout.contains(secret), "{stdout}");
        }
        assert_eq!(std::fs::read(&path).unwrap(), contents.as_bytes());
    }
}
