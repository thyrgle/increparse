//! Parse off the hot thread.
//!
//! [`BackgroundRunner`] owns a worker thread and a [`Session`] for one
//! document. You feed it the latest source text; it runs the engine off the
//! caller's thread and hands the settled tree back through a callback. The
//! thread behind a `serve` loop never blocks on parsing, no matter how large
//! the document grows.
//!
//! The runner is deliberately small and framework-agnostic — no async
//! runtime, no I/O, no protocol. It cooperates with whatever loop you have:
//!
//! * **Coalescing** — submissions that queue up while a run is in flight are
//!   drained and collapsed into the newest one. A burst of keystrokes
//!   produces at most two runs: the one already in flight, and one final run
//!   for the latest state.
//! * **Reuse** — the worker keeps the [`Session`] between runs, so each run
//!   reuses everything the edit did not touch. A changed root context (or a
//!   cancelled run) rebuilds from scratch.
//! * **Cancellation** — [`cancel`](BackgroundRunner::cancel) fires the
//!   shared [`CancelToken`], which the engine checks between batches. A run
//!   ended this way reports `cancelled: true` and the tree may be partially
//!   updated; skip publishing diagnostics for it and resubmit.
//!
//! One runner per document: the incremental state it keeps is per-file.
//!
//! # Examples
//!
//! ```
//! use std::sync::mpsc;
//!
//! use increparse::{Engine, Outcome, Pass, Span};
//! use increparse_lsp::BackgroundRunner;
//!
//! #[derive(Clone, Debug, PartialEq, Eq)]
//! enum Ctx {
//!     Doc,
//! }
//!
//! struct Accept;
//! impl Pass for Accept {
//!     type Ctx = Ctx;
//!     fn parse(&self, _source: &str, _span: Span, _ctx: &Ctx) -> Outcome<Ctx> {
//!         Outcome::Done
//!     }
//! }
//!
//! let (tx, rx) = mpsc::channel();
//! let runner = BackgroundRunner::spawn(Engine::with((Accept,)), move |tree, report, revision| {
//!     let _ = tx.send((report.reached_fixpoint, revision));
//! });
//! runner.submit("hello".into(), Ctx::Doc, 1);
//! assert_eq!(rx.recv().unwrap(), (true, 1));
//! runner.shutdown();
//! ```

use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use increparse::{CancelToken, Engine, ParseTree, RunReport, SerialExecutor, Session};

/// A queued parse: the source text, the root context to parse it under, and
/// the document revision the text corresponds to.
struct Job<C> {
    source: String,
    root_ctx: C,
    revision: u64,
}

/// Runs parses for one document on a dedicated worker thread.
///
/// Spawn one with [`spawn`](BackgroundRunner::spawn), feed it the latest
/// source with [`submit`](BackgroundRunner::submit), and consume results in
/// the callback. See the [module docs](self) for the coalescing, reuse, and
/// cancellation contract.
pub struct BackgroundRunner<C> {
    tx: Option<Sender<Job<C>>>,
    handle: Option<JoinHandle<()>>,
    cancel: Arc<CancelToken>,
}

impl<C> BackgroundRunner<C>
where
    C: Clone + PartialEq + Send + 'static,
{
    /// Spawns the worker thread.
    ///
    /// `on_result` is called on the worker thread after each run, with the
    /// settled tree, the run report, and the revision that was parsed. Keep
    /// it fast — it blocks the next queued run. Check
    /// [`RunReport::cancelled`] before publishing: a cancelled run leaves
    /// the tree partially updated.
    pub fn spawn(
        engine: Engine<C>,
        on_result: impl Fn(&ParseTree<C>, &RunReport, u64) + Send + Sync + 'static,
    ) -> Self {
        let (tx, rx) = channel::<Job<C>>();
        let cancel = Arc::new(CancelToken::new());
        let worker_cancel = Arc::clone(&cancel);
        let handle = thread::spawn(move || {
            let mut session: Option<Session<C>> = None;
            // The (source, root context) the session currently reflects.
            let mut current: Option<(String, C)> = None;
            // Whether the session needs a run: true after an edit, a
            // context change, or a cancelled run.
            let mut dirty = true;

            while let Ok(mut job) = rx.recv() {
                // Coalesce everything queued while we were idle into the
                // newest job; only the latest state matters.
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }

                let same_state = matches!(
                    &current,
                    Some((source, ctx))
                        if *source == job.source && *ctx == job.root_ctx
                );
                if !same_state {
                    let ctx_unchanged = matches!(&current, Some((_, ctx)) if *ctx == job.root_ctx);
                    if ctx_unchanged {
                        let old = current.as_ref().map(|(source, _)| source).expect("checked");
                        let edit = diff_edit(old, &job.source);
                        session.as_mut().expect("checked").edit(edit);
                    } else {
                        session = Some(Session::from_source(
                            &job.source,
                            job.revision,
                            job.root_ctx.clone(),
                        ));
                    }
                    current = Some((job.source.clone(), job.root_ctx));
                    dirty = true;
                }

                if dirty {
                    worker_cancel.reset();
                    let session = session.as_mut().expect("session exists before a run");
                    let report = engine.run(
                        &job.source,
                        session.tree_mut(),
                        &SerialExecutor,
                        &worker_cancel,
                    );
                    // A cancelled run leaves the tree partially updated:
                    // mark dirty so the next submission re-runs even if the
                    // source is identical.
                    dirty = report.cancelled;
                    on_result(session.tree(), &report, job.revision);
                }
            }
        });
        Self {
            tx: Some(tx),
            handle: Some(handle),
            cancel,
        }
    }

    /// Submits the latest source for parsing.
    ///
    /// Coalesces with any job queued but not yet started: only the newest
    /// submission runs. A submission identical to the last *settled* state
    /// (same source, same root context, previous run not cancelled) is a
    /// no-op — no run, no callback.
    pub fn submit(&self, source: String, root_ctx: C, revision: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job {
                source,
                root_ctx,
                revision,
            });
        }
    }

    /// Cancels the run in flight, if any.
    ///
    /// The engine checks the token between batches, so a run ends at batch
    /// granularity with `RunReport::cancelled` set. The next submission
    /// resets the token and parses normally. Cancelling while idle is
    /// harmless.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Drops queued submissions and waits for the worker to exit.
    ///
    /// A run already in flight finishes first. Dropping without calling
    /// this is fine too: the worker exits on its own once the runner (and
    /// any cloned handles) are gone — it is just not joined.
    pub fn shutdown(mut self) {
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Computes the smallest byte-range [`Edit`] turning `old` into `new`.
///
/// Walks the common byte prefix and suffix (backed off to UTF-8 character
/// boundaries) and replaces everything between. Minimal by construction:
/// identical text yields the no-op `Edit::replace(len, len, len)`, a pure
/// insert or delete touches only the changed range.
pub(crate) fn diff_edit(old: &str, new: &str) -> increparse::Edit {
    let (start, old_end, new_end) = diff_bounds(old.as_bytes(), new.as_bytes(), old, new);
    increparse::Edit::replace(start, old_end, new_end)
}

fn diff_bounds(old: &[u8], new: &[u8], old_str: &str, new_str: &str) -> (usize, usize, usize) {
    let mut start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    while start > 0 && (!old_str.is_char_boundary(start) || !new_str.is_char_boundary(start)) {
        start -= 1;
    }
    let max_suffix = (old.len() - start).min(new.len() - start);
    let mut suffix = old[old.len() - max_suffix..]
        .iter()
        .rev()
        .zip(new[new.len() - max_suffix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while suffix > 0
        && (!old_str.is_char_boundary(old.len() - suffix)
            || !new_str.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }
    (start, old.len() - suffix, new.len() - suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(old: &str, new: &str) -> (usize, usize, usize) {
        diff_bounds(old.as_bytes(), new.as_bytes(), old, new)
    }

    #[test]
    fn diff_identical_is_noop_replacement() {
        // The whole common prefix is consumed; no suffix overlaps it.
        assert_eq!(bounds("same", "same"), (4, 4, 4));
        assert_eq!(bounds("", ""), (0, 0, 0));
    }

    #[test]
    fn diff_pure_insert() {
        // "ab" -> "aXb": insert at 1.
        assert_eq!(bounds("ab", "aXb"), (1, 1, 2));
        // Append and prepend.
        assert_eq!(bounds("ab", "abXYZ"), (2, 2, 5));
        assert_eq!(bounds("ab", "XYZab"), (0, 0, 3));
    }

    #[test]
    fn diff_pure_delete() {
        assert_eq!(bounds("aXb", "ab"), (1, 2, 1));
        assert_eq!(bounds("abXYZ", "ab"), (2, 5, 2));
    }

    #[test]
    fn diff_replace_middle() {
        // Only the differing final byte is replaced.
        assert_eq!(bounds("foo bar", "foo baz"), (6, 7, 7));
    }

    #[test]
    fn diff_respects_char_boundaries() {
        // 'é' is two bytes; a byte-level prefix would land inside it.
        assert_eq!(bounds("é", "e"), (0, 2, 1));
        // A shared lead byte (0xC3) must not split the character either.
        assert_eq!(bounds("éx", "êx"), (0, 2, 2));
        // Multibyte suffix.
        assert_eq!(bounds("hé", "xé"), (0, 1, 1));
    }

    #[test]
    fn diff_edit_applies_through_session() {
        for (old, new) in [
            ("hello", "hey"),
            ("", "content"),
            ("content", ""),
            ("héllo wörld", "héy wörld"),
        ] {
            let mut session = Session::from_source(old, 0, ());
            session.edit(diff_edit(old, new));
            assert_eq!(
                session.tree().span(session.tree().root()).end,
                new.len(),
                "{old:?} -> {new:?}"
            );
        }
    }
}
