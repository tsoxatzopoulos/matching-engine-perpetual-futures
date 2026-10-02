//! Determinism golden test.
//!
//! Replays a fixed pseudo-random workload (~200k commands) and compares the
//! hash of the full event stream against `tests/golden/events.txt`, which was
//! recorded from the reference implementation. Any change to engine behaviour,
//! including event order, shows up here.
//!
//! Re-record (only when a behaviour change is intended):
//!     GOLDEN_BLESS=1 cargo test --release --test golden

use std::fmt::Write as _;

use futures_engine::sim::{Fnv64, GoldenWorkload};
use futures_engine::*;

const SEED: u64 = 0x5EED_2026;
const ACCOUNTS: u64 = 400;
const COMMANDS: usize = 200_000;
const CHUNK: usize = 10_000;
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/events.txt");

#[derive(Default, Debug)]
struct Stats {
    events: usize,
    trades: usize,
    liquidations: usize,
    adl: usize,
    triggered: usize,
    funding: usize,
    order_rejects: usize,
    command_rejects: usize,
    insurance_changes: usize,
}

struct Recorder {
    line: String,
    chunk: Fnv64,
    chunk_events: usize,
    total: Fnv64,
    stats: Stats,
    lines: Vec<String>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            line: String::new(),
            chunk: Fnv64::default(),
            chunk_events: 0,
            total: Fnv64::default(),
            stats: Stats::default(),
            lines: Vec::new(),
        }
    }

    fn record(&mut self, events: Vec<Event>) {
        for ev in &events {
            self.line.clear();
            writeln!(self.line, "{ev:?}").unwrap();
            self.chunk.write(self.line.as_bytes());
            self.total.write(self.line.as_bytes());
            self.chunk_events += 1;
            let s = &mut self.stats;
            s.events += 1;
            match ev {
                Event::Trade { .. } => s.trades += 1,
                Event::Liquidation { .. } => s.liquidations += 1,
                Event::AutoDeleverage { .. } => s.adl += 1,
                Event::OrderTriggered { .. } => s.triggered += 1,
                Event::FundingPayment { .. } => s.funding += 1,
                Event::OrderRejected { .. } => s.order_rejects += 1,
                Event::CommandRejected { .. } => s.command_rejects += 1,
                Event::InsuranceFundChanged { .. } => s.insurance_changes += 1,
                _ => {}
            }
        }
    }

    fn close_chunk(&mut self, label: String) {
        let hash = std::mem::take(&mut self.chunk).finish();
        self.lines.push(format!("{label} {hash:016x} {}", self.chunk_events));
        self.chunk_events = 0;
    }
}

fn run(commands: usize) -> Recorder {
    run_with(commands, RearmPolicy::OnMark, 0)
}

/// `idle_rearm > 0` calls `rearm_pending(idle_rearm)` between commands, like
/// the threaded runtime does when it has nothing to do.
fn run_with(commands: usize, policy: RearmPolicy, idle_rearm: usize) -> Recorder {
    let mut engine = Engine::new();
    engine.set_rearm_policy(policy);
    let mut workload = GoldenWorkload::new(SEED, ACCOUNTS);
    let mut rec = Recorder::new();
    for cmd in workload.setup() {
        rec.record(engine.process(cmd));
    }
    rec.close_chunk("setup".into());
    for i in 0..commands {
        let cmd = workload.next(&engine);
        rec.record(engine.process(cmd));
        if idle_rearm > 0 && i % 3 == 0 {
            engine.rearm_pending(idle_rearm);
        }
        if (i + 1) % CHUNK == 0 {
            rec.close_chunk(format!("chunk {:>3}", i / CHUNK));
        }
    }
    let total = rec.total.finish();
    let events = rec.stats.events;
    rec.lines.push(format!("total {total:016x} {events}"));
    rec
}

#[test]
fn golden_event_stream() {
    let rec = run(COMMANDS);
    let s = &rec.stats;
    println!("{s:#?}");

    // The workload must actually exercise the risky paths.
    assert!(s.trades > 10_000, "too few trades: {}", s.trades);
    assert!(s.liquidations > 0, "no liquidations");
    assert!(s.adl > 0, "no auto-deleveraging");
    assert!(s.triggered > 0, "no conditional triggers");
    assert!(s.funding > 0, "no funding payments");

    let header = format!("# seed={SEED:#x} accounts={ACCOUNTS} commands={COMMANDS} chunk={CHUNK}");
    let actual: Vec<String> = std::iter::once(header).chain(rec.lines.iter().cloned()).collect();

    if std::env::var_os("GOLDEN_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, actual.join("\n") + "\n").unwrap();
        println!("golden file written: {GOLDEN}");
        return;
    }

    let expected = std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|_| panic!("missing {GOLDEN}; record it with GOLDEN_BLESS=1"));
    let expected: Vec<&str> = expected.lines().collect();
    for (i, (want, got)) in expected.iter().zip(&actual).enumerate() {
        assert_eq!(
            want, got,
            "event stream diverges at line {i} of the golden file (first differing chunk)"
        );
    }
    assert_eq!(expected.len(), actual.len(), "golden file has a different number of chunks");
}

/// Two runs in the same process must agree (catches hash-seed dependent order).
#[test]
fn same_process_runs_agree() {
    let a = run(30_000);
    let b = run(30_000);
    assert_eq!(a.lines, b.lines);
}

/// Re-arm policies only move work around: the event stream must not change.
#[test]
fn rearm_policies_do_not_change_events() {
    const PREFIX: usize = 60_000;
    let reference = run(PREFIX);
    let variants = [
        ("per-command", RearmPolicy::PerCommand, 0),
        ("threshold", RearmPolicy::Threshold(50), 0),
        ("idle", RearmPolicy::OnMark, 7),
    ];
    for (name, policy, idle) in variants {
        let rec = run_with(PREFIX, policy, idle);
        assert_eq!(rec.lines, reference.lines, "policy {name} changed the event stream");
    }
}
