//! Release-build measurement of the external backend's memory worst case.
//!
//! Every session floods updates at downstream clients that never read, so
//! each queue fills until the session fails closed. The peak private memory
//! of this process while that happens is the number the bounds are sized by.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use kage_acp::acp::{ContentBlock, PromptRequest};
use kage_core::sync::lock;

use super::super::MAX_SESSIONS;
use super::super::lifecycle::Lifecycles;
use super::{External, Launch, MAX_ATTACHMENTS, Recording};

/// Stalled downstream clients attached to each session.
const CLIENTS: u64 = 4;

/// Private memory of this process, in KiB.
fn rss_anon_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("RssAnon:"))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

/// Samples [`rss_anon_kib`] until dropped and keeps the highest reading.
struct PeakSampler {
    peak: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl PeakSampler {
    fn start() -> Self {
        let peak = Arc::new(AtomicU64::new(rss_anon_kib()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (peak, stop) = (Arc::clone(&peak), Arc::clone(&stop));
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    peak.fetch_max(rss_anon_kib(), Ordering::AcqRel);
                    thread::sleep(Duration::from_millis(2));
                }
            })
        };
        Self {
            peak,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> u64 {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
        self.peak.load(Ordering::Acquire)
    }
}

/// A downstream peer whose far end is never read.
fn stalled_peer(kept: &mut Vec<UnixStream>) -> kage_acp::agent::AgentPeer {
    let (client, server) = UnixStream::pair().unwrap();
    kept.push(client);
    let (peer, inbox, _) =
        kage_jsonrpc::connect(BufReader::new(server.try_clone().unwrap()), server);
    std::mem::forget(inbox);
    peer.into()
}

fn flooding_agent(extra_env: &[(&str, &str)]) -> kage_core::config::AcpAgent {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/external_flood.py");
    kage_core::config::AcpAgent {
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        env: extra_env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// Starts `MAX_SESSIONS` recorded sessions with `clients` stalled clients
/// each, and with `flood` prompts all of them to flood at once. Returns the
/// highest private memory seen while they ran, with how many sessions failed
/// closed. Without `flood` it returns the memory once the sessions are idle.
fn flood_sessions(config: &kage_core::config::AcpAgent, clients: u64, flood: bool) -> (u64, usize) {
    let dir = tempfile::tempdir().unwrap();
    let lifecycles = Lifecycles::new(MAX_SESSIONS);
    let mut kept = Vec::new();
    let peers: Vec<_> = (0..clients).map(|_| stalled_peer(&mut kept)).collect();
    let mut sessions = Vec::new();
    for n in 0..MAX_SESSIONS {
        let (path, mut header) = crate::plan_session_in(dir.path(), "external", "");
        header.cwd = dir.path().to_path_buf();
        let (session, created) = External::start(
            &lifecycles,
            Launch {
                id: format!("local-{n}"),
                name: "flood",
                cwd: dir.path().to_str().unwrap(),
                config,
                owner: 1,
                peer: peers[0].clone(),
                record: Some(Recording::Create {
                    path,
                    header: Box::new(header),
                }),
                restore: None,
            },
        )
        .unwrap();
        created.announce();
        for (owner, peer) in (2..).zip(&peers[1..]) {
            session.attach(owner, peer.clone()).unwrap();
            session.announce(owner);
        }
        sessions.push(session);
    }
    if !flood {
        thread::sleep(Duration::from_secs(1));
        return (rss_anon_kib(), 0);
    }
    let sampler = PeakSampler::start();
    let start = Arc::new(Barrier::new(sessions.len() + 1));
    let turns: Vec<_> = sessions
        .iter()
        .enumerate()
        .map(|(n, session)| {
            let (session, start) = (Arc::clone(session), Arc::clone(&start));
            thread::spawn(move || {
                let ticket = session.register_prompt(1).unwrap();
                start.wait();
                let _ = session.prompt(
                    ticket,
                    PromptRequest {
                        session_id: format!("local-{n}"),
                        prompt: vec![ContentBlock::text("flood")],
                        delivery: None,
                    },
                );
            })
        })
        .collect();
    start.wait();
    let deadline = Instant::now() + Duration::from_secs(120);
    for turn in turns {
        while !turn.is_finished() {
            assert!(Instant::now() < deadline, "a flooded turn never ended");
            thread::sleep(Duration::from_millis(20));
        }
        turn.join().unwrap();
    }
    let peak = sampler.finish();
    let closed = sessions
        .iter()
        .filter(|session| lock(&session.state).closed)
        .count();
    (peak, closed)
}

/// The external backend's memory at its bounds, measured against the
/// process's private memory before it starts anything.
///
/// Idle, every slot with every attachment costs the writer threads and their
/// queues: one release run measured 15 MiB over the baseline (AMD Ryzen 7
/// 7700, Linux `x86_64`, 8 slots of 8 attachments), against 97 MiB for 16
/// slots of 32. Flooded, every slot has four stalled clients and floods until
/// its session fails closed: the run measured 60 MiB over the baseline with 4
/// MiB of buffered updates and 1 MiB each of recording and display history
/// per session, against 187 MiB for 16 slots with 8 MiB and 4 MiB. Each
/// budget is twice its measurement.
#[test]
#[ignore = "measures a release build: cargo test --release -p kage-cli --bin kage -- --ignored external_backend"]
fn the_external_backend_stays_within_its_memory_budget() {
    const IDLE_BUDGET_KIB: u64 = 30 * 1024;
    const FLOODED_BUDGET_KIB: u64 = 120 * 1024;
    let baseline = rss_anon_kib();
    let (idle, _) = flood_sessions(&flooding_agent(&[]), MAX_ATTACHMENTS as u64, false);
    let idle = idle.saturating_sub(baseline);
    let (peak, closed) = flood_sessions(&flooding_agent(&[]), CLIENTS, true);
    let flooded = peak.saturating_sub(baseline);
    eprintln!("idle external backend RssAnon: {idle} KiB over {baseline} KiB");
    eprintln!("flooded external backend RssAnon: {flooded} KiB over {baseline} KiB");
    assert_eq!(
        closed, MAX_SESSIONS,
        "every session must reach its bound for this to be the worst case"
    );
    assert!(
        idle <= IDLE_BUDGET_KIB,
        "idle attachments use {idle} KiB, over {IDLE_BUDGET_KIB} KiB"
    );
    assert!(
        flooded <= FLOODED_BUDGET_KIB,
        "the flooded backend uses {flooded} KiB, over {FLOODED_BUDGET_KIB} KiB"
    );
}
