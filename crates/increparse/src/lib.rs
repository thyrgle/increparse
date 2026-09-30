//! Multi-pass fixpoint parsing for editors, LSPs, and compilers.
//!
//! `increparse` is **not** a parser combinator library. It is the missing
//! piece *around* one: an engine that executes **schedules of passes** over a
//! growing parse tree until the tree settles into a fixpoint.
//!
//! # The idea
//!
//! Parsing a real program in one monolithic sweep is brittle — one syntax
//! error in a function body can hide the whole file's structure. Instead,
//! `increparse` lets you parse in widening rounds of understanding:
//!
//! * **Round 0** runs a cheap, error-tolerant pass over the whole file that
//!   matches coarse structure, e.g. `def name(params) { * }`, where `*` is an
//!   explicit *hole*: a child region the pass could not or did not parse.
//! * **Round 1** runs a deeper pass over each hole, with the round-0 context
//!   threaded in (`name`, `params`), expanding statements inside bodies.
//! * Later rounds keep expanding until every region is either accepted or
//!   permanently failed.
//!
//! Because failed regions stay in the tree as leaves, consumers — an LSP
//! answering "what functions does this file define?" for example — still see
//! the coarse structure even when deep passes are still failing. That is the
//! error-resilience payoff.
//!
//! # Incremental edits
//!
//! The tree is built to be re-parsed, not rebuilt. [`Session`] wraps a
//! [`ParseTree`] that can absorb [`Edit`]s: every span is remapped into the
//! new coordinates and only the nodes the edit touched are reset for
//! re-parsing. When the next run re-expands their parents, produced children
//! are matched against the surviving ones by span and context — equal
//! children keep their identity, their status, and their whole subtree. An
//! edit inside one function body re-parses that function; every other
//! function's subtree is carried over untouched.
//!
//! # The moving parts
//!
//! | Piece | Role |
//! |-------|------|
//! | [`Pass`] | Your parse logic: `(source, span, ctx) -> Outcome<Ctx>`. Wraps any combinator (`nom`, `chumsky`, PEG, regex, hand-rolled). |
//! | [`Schedule`] | Ordered passes; round `r` uses pass `r`. |
//! | [`ParseTree`] | Arena of `(span, ctx, status)` nodes; the result of a run. |
//! | [`Engine`] | Drives rounds to a fixpoint, with cancellation and pluggable parallelism. |
//! | [`Executor`] | How a round's batch runs: [`SerialExecutor`] (default) or a rayon-backed executor (feature `parallel`). |
//! | [`CancelToken`] | Cooperative cancellation at job granularity — built for LSP-style "user typed again" restarts. |
//!
//! # Termination by construction
//!
//! Passes may only produce child spans **contained in** their parent and on
//! the same source revision; the engine rejects any outcome that violates
//! this, marking the node failed. Since round `r` only processes nodes at
//! depth `r` and runs are capped at one round per scheduled pass, every node
//! is processed at most once per pass and a run performs at most
//! `schedule.len()` rounds — no schedule can loop forever. (For passes that
//! should always *divide* their input, [`EngineConfig::enforce_shrink`] adds
//! a strict "children must be smaller" rule on top.)
//!
//! # A taste
//!
//! A tiny language of function definitions, parsed in three passes. (The
//! full, runnable version with real scanning is in `examples/mini_lang.rs`.)
//!
//! ```
//! use increparse::{Engine, Outcome, ParseTree, Pass, Schedule, SerialExecutor, Span, Status};
//!
//! #[derive(Clone, Debug, PartialEq, Eq)]
//! enum Ctx {
//!     File,
//!     Function { name: String },
//! }
//!
//! /// Round 0: accept the whole file. (A real pass would scan for `def`s
//! /// and expand each one into a child region.)
//! struct Functions;
//!
//! impl Pass for Functions {
//!     type Ctx = Ctx;
//!
//!     fn parse(&self, _source: &str, span: Span, ctx: &Ctx) -> Outcome<Ctx> {
//!         match ctx {
//!             Ctx::File => Outcome::Expand(vec![(
//!                 Span::new(span.start + 4, span.end, span.rev),
//!                 Ctx::Function { name: "main".into() },
//!             )]),
//!             Ctx::Function { .. } => Outcome::Done,
//!         }
//!     }
//! }
//!
//! # fn main() {
//! // Round 0 finds the function region; round 1 re-runs the pass on the
//! // nodes round 0 created (children always start one round deeper).
//! let engine = Engine::with((Functions, Functions));
//!
//! let source = "def main() { }";
//! let mut tree = ParseTree::from_source(source, 0, Ctx::File);
//! let report = engine.run(source, &mut tree, &SerialExecutor, &increparse::CancelToken::new());
//!
//! assert!(report.reached_fixpoint);
//! assert_eq!(tree.status(tree.root()), Status::Expanded);
//! let func = tree.children(tree.root())[0];
//! assert_eq!(tree.ctx(func), &Ctx::Function { name: "main".into() });
//! assert_eq!(tree.status(func), Status::Done);
//! # }
//! ```
//!
//! # Roadmap
//!
//! * `increparse-lsp`: an adapter crate wiring the engine into an LSP server
//!   loop with background runs and cancellation between batches.
//! * Finer-grained reuse hooks (e.g. matching by user-supplied keys instead
//!   of `PartialEq`).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod cancel;
mod engine;
mod executor;
mod job;
mod node;
mod outcome;
mod pass;
mod run;
mod schedule;
mod session;
mod span;
mod status;
mod tree;

pub use cancel::CancelToken;
pub use engine::{Engine, EngineConfig, Passes, RunReport, Violation, ViolationKind};
pub use executor::{Executor, SerialExecutor};
pub use job::Job;
pub use node::NodeId;
pub use outcome::Outcome;
pub use pass::{pass_fn, Pass, PassFn};
pub use run::run;
pub use schedule::Schedule;
pub use session::{Edit, Session};
pub use span::Span;
pub use status::{Status, StatusCounts};
pub use tree::ParseTree;

/// The types you almost always want, in one glob.
pub mod prelude {
    #[cfg(feature = "parallel")]
    pub use crate::RayonExecutor;
    pub use crate::{
        pass_fn, run, CancelToken, Engine, EngineConfig, Executor, Job, NodeId, Outcome, ParseTree,
        Pass, RunReport, Schedule, SerialExecutor, Session, Span, Status, StatusCounts, Violation,
        ViolationKind,
    };
}

#[cfg(feature = "parallel")]
pub use executor_rayon::RayonExecutor;
#[cfg(feature = "parallel")]
mod executor_rayon;
