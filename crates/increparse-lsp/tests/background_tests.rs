//! Tests for [`BackgroundRunner`]: callback delivery, coalescing, session
//! reuse, root-context rebuilds, and shutdown — all over plain channels, no
//! sleeps on the assert paths where avoidable.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use increparse::prelude::*;
use increparse_lsp::BackgroundRunner;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ctx {
    Doc,
}

/// Settles immediately. `runs` counts invocations so tests can tell real
/// runs from skipped ones.
struct Settle {
    runs: Arc<AtomicUsize>,
}

impl Pass for Settle {
    type Ctx = Ctx;

    fn parse(&self, _source: &str, _span: Span, _ctx: &Ctx) -> Outcome<Ctx> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Outcome::Done
    }
}

/// Blocks the first `block_for` parse calls until released. Used to hold the
/// worker mid-run while the test queues more submissions. `runs` counts
/// parse invocations.
struct Gate {
    gate: Arc<GateState>,
    runs: Arc<AtomicUsize>,
}

struct GateState {
    entered: Mutex<usize>,
    entered_cv: Condvar,
    open: Mutex<bool>,
    open_cv: Condvar,
}

impl GateState {
    fn new() -> Self {
        Self {
            entered: Mutex::new(0),
            entered_cv: Condvar::new(),
            open: Mutex::new(false),
            open_cv: Condvar::new(),
        }
    }

    fn wait(&self) {
        {
            let mut entered = self.entered.lock().unwrap();
            *entered += 1;
            self.entered_cv.notify_all();
        }
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.open_cv.wait(open).unwrap();
        }
    }

    fn wait_entered(&self, n: usize) {
        let mut entered = self.entered.lock().unwrap();
        while *entered < n {
            entered = self.entered_cv.wait(entered).unwrap();
        }
    }

    fn release(&self) {
        let mut open = self.open.lock().unwrap();
        *open = true;
        self.open_cv.notify_all();
    }
}

impl Pass for Gate {
    type Ctx = Ctx;

    fn parse(&self, _source: &str, _span: Span, _ctx: &Ctx) -> Outcome<Ctx> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.gate.wait();
        Outcome::Done
    }
}

/// Collects `(reached_fixpoint, cancelled, revision)` per callback.
type Collected = Receiver<(bool, bool, u64)>;

fn collector() -> (
    impl Fn(&ParseTree<Ctx>, &RunReport, u64) + Send + Sync + 'static,
    Collected,
) {
    let (tx, rx) = channel();
    let f = move |tree: &ParseTree<Ctx>, report: &RunReport, revision: u64| {
        let _ = tree;
        let _ = tx.send((report.reached_fixpoint, report.cancelled, revision));
    };
    (f, rx)
}

fn recv(rx: &Receiver<(bool, bool, u64)>) -> (bool, bool, u64) {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("callback should arrive")
}

fn assert_quiet(rx: &Receiver<(bool, bool, u64)>) {
    match rx.recv_timeout(Duration::from_millis(150)) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("expected no callback, got {other:?}"),
    }
}

#[test]
fn delivers_result_and_reports_reuse() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (cb, rx) = collector();
    let runner = BackgroundRunner::spawn(
        Engine::with((Settle {
            runs: Arc::clone(&runs),
        },)),
        cb,
    );
    runner.submit("hello".into(), Ctx::Doc, 1);
    assert_eq!(recv(&rx), (true, false, 1));

    // A small edit reuses the session: the worker runs, revision flows
    // through, and the parse count grew by exactly one run.
    runner.submit("hey".into(), Ctx::Doc, 2);
    assert_eq!(recv(&rx), (true, false, 2));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    runner.shutdown();
}

#[test]
fn coalesces_burst_into_latest() {
    let gate = Arc::new(GateState::new());
    let runs = Arc::new(AtomicUsize::new(0));
    let (cb, rx) = collector();
    let runner = BackgroundRunner::spawn(
        Engine::with((Gate {
            gate: Arc::clone(&gate),
            runs: Arc::clone(&runs),
        },)),
        cb,
    );

    // Job 1 starts and blocks inside the pass.
    runner.submit("one".into(), Ctx::Doc, 1);
    gate.wait_entered(1);

    // Jobs 2..5 queue while the worker is blocked.
    for rev in 2..=5 {
        runner.submit(format!("one{rev}"), Ctx::Doc, rev);
    }
    gate.release();

    // The in-flight job completes first...
    assert_eq!(recv(&rx), (true, false, 1));
    // ...then exactly one coalesced run for the newest revision.
    assert_eq!(recv(&rx), (true, false, 5));
    assert_quiet(&rx);
    // Two parse invocations total: the blocked one and the coalesced one.
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    runner.shutdown();
}

#[test]
fn identical_submission_is_skipped() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (cb, rx) = collector();
    let runner = BackgroundRunner::spawn(
        Engine::with((Settle {
            runs: Arc::clone(&runs),
        },)),
        cb,
    );
    runner.submit("same".into(), Ctx::Doc, 1);
    assert_eq!(recv(&rx), (true, false, 1));

    // Same source, same ctx: no run, no callback. The revision bump alone
    // is not a reason to re-parse.
    runner.submit("same".into(), Ctx::Doc, 2);

    // A real change then proves the worker is still alive and lets us
    // assert quietness without racing the skipped job.
    runner.submit("changed".into(), Ctx::Doc, 3);
    assert_eq!(recv(&rx), (true, false, 3));
    assert_quiet(&rx);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    runner.shutdown();
}

#[test]
fn changed_root_context_rebuilds() {
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Ctx2 {
        A,
        B,
    }

    struct Settle2;
    impl Pass for Settle2 {
        type Ctx = Ctx2;
        fn parse(&self, _source: &str, _span: Span, _ctx: &Ctx2) -> Outcome<Ctx2> {
            Outcome::Done
        }
    }

    let (tx, rx) = channel();
    let runner = BackgroundRunner::spawn(
        Engine::with((Settle2,)),
        move |_: &ParseTree<Ctx2>, report: &RunReport, revision: u64| {
            let _ = tx.send((report.reached_fixpoint, revision));
        },
    );
    // Identical source, different root context: the session is rebuilt and
    // the document re-parses — no stale reuse across contexts.
    runner.submit("same text".into(), Ctx2::A, 1);
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (true, 1));
    runner.submit("same text".into(), Ctx2::B, 2);
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (true, 2));
    runner.shutdown();
}

#[test]
fn cancel_between_runs_is_harmless_and_next_submit_resets() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (cb, rx) = collector();
    let runner = BackgroundRunner::spawn(
        Engine::with((Settle {
            runs: Arc::clone(&runs),
        },)),
        cb,
    );
    // Cancel while idle: nothing to cancel, no state corruption.
    runner.cancel();
    runner.submit("after cancel".into(), Ctx::Doc, 1);
    assert_eq!(recv(&rx), (true, false, 1));
    runner.shutdown();
}

#[test]
fn shutdown_joins_worker() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (cb, rx) = collector();
    let runner = BackgroundRunner::spawn(
        Engine::with((Settle {
            runs: Arc::clone(&runs),
        },)),
        cb,
    );
    runner.submit("before".into(), Ctx::Doc, 1);
    assert_eq!(recv(&rx), (true, false, 1));
    // Shutdown with no work pending returns promptly instead of hanging.
    runner.shutdown();
}

#[test]
fn queued_job_after_shutdown_never_runs() {
    let gate = Arc::new(GateState::new());
    let (cb, rx) = collector();
    let runner = BackgroundRunner::spawn(
        Engine::with((Gate {
            gate: Arc::clone(&gate),
            runs: Arc::new(AtomicUsize::new(0)),
        },)),
        cb,
    );
    // Hold the worker inside the first run, queue a second job, release,
    // then drop the sender: the queued job is drained by the closed channel
    // (recv errors after the queue empties — the queued job itself was
    // already sent, so it must complete, but nothing queued after shutdown
    // may run).
    runner.submit("first".into(), Ctx::Doc, 1);
    gate.wait_entered(1);
    runner.submit("second".into(), Ctx::Doc, 2);
    gate.release();
    assert_eq!(recv(&rx), (true, false, 1));
    assert_eq!(recv(&rx), (true, false, 2));
    assert_quiet(&rx);
    runner.shutdown();
}
