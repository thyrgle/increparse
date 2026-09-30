//! Pluggable batch execution of jobs.

use crate::cancel::CancelToken;
use crate::job::Job;
use crate::outcome::Outcome;

/// Runs a batch of jobs, possibly in parallel.
///
/// The engine collects the ready nodes of a round into jobs, hands the batch
/// to an executor together with a pure `run` closure, and merges the results
/// back into the tree in job order — so runs are deterministic regardless of
/// the executor used.
///
/// # Contract
///
/// Implementations must return the executed jobs paired with their outcomes,
/// **in input order**, and may stop early: the returned vector may be a
/// prefix (possibly empty) of the input batch. The engine treats a short
/// result as a cancellation and leaves the un-merged nodes untouched for a
/// later run.
///
/// # Provided implementations
///
/// * [`SerialExecutor`] — runs jobs one at a time (default, zero cost).
/// * The rayon-backed executor — runs jobs on a thread pool (requires the
///   `parallel` feature).
pub trait Executor: Send + Sync {
    /// Executes `jobs` with `run`, honouring `cancel` at job granularity.
    fn execute<C, F>(
        &self,
        jobs: Vec<Job<C>>,
        run: F,
        cancel: &CancelToken,
    ) -> Vec<(Job<C>, Outcome<C>)>
    where
        C: Send + 'static,
        F: Fn(&Job<C>) -> Outcome<C> + Send + Sync;
}

/// An [`Executor`] that runs jobs one at a time on the calling thread.
///
/// # Examples
///
/// ```
/// use increparse::{CancelToken, Executor, Job, Outcome, SerialExecutor, Span};
///
/// let exec = SerialExecutor;
/// let jobs = vec![Job { node: increparse::NodeId(0), span: Span::new(0, 1, 0), ctx: (), pass_index: 0 }];
/// let out = exec.execute(jobs, |_| Outcome::Done, &CancelToken::new());
/// assert_eq!(out.len(), 1);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct SerialExecutor;

impl Executor for SerialExecutor {
    fn execute<C, F>(
        &self,
        jobs: Vec<Job<C>>,
        run: F,
        cancel: &CancelToken,
    ) -> Vec<(Job<C>, Outcome<C>)>
    where
        C: Send + 'static,
        F: Fn(&Job<C>) -> Outcome<C> + Send + Sync,
    {
        let mut executed = Vec::with_capacity(jobs.len());
        for job in jobs {
            if cancel.is_cancelled() {
                break;
            }
            let outcome = run(&job);
            executed.push((job, outcome));
        }
        executed
    }
}
