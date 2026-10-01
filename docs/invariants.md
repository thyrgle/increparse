# Engine invariants

What `increparse` guarantees, and the test that enforces each guarantee.
Every claim here is executable — if a test name below fails, the
corresponding line in this document is wrong, not the test.

All properties live in `crates/increparse/tests/properties.rs` unless
noted. Property tests run 512 randomized cases per invocation, over
grammars that include nested brace groups up to depth four and multibyte
characters (`日本`, `é`) at edit targets.

## Core semantics

| Invariant | Enforced by |
|---|---|
| Runs terminate: every run ends at fixpoint, budget exhaustion, or cancellation — never a hang or panic. | `terminates_and_tree_is_wellformed` |
| Trees are well-formed: spans ordered and in bounds, children inside parents, root covers the source, status counts total the arena. | `assert_wellformed`, called by every property |
| Cold parses are deterministic: two runs over the same input produce identical trees. | `runs_are_deterministic` |
| Contract-violating children (outside parent, wrong revision, not smaller under `enforce_shrink`) are rejected and reported, never propagated into the tree. | `escaping_children_are_rejected_and_reported`, `zero_width_and_wrong_revision_children_are_handled` |

## Incremental correctness

| Invariant | Enforced by |
|---|---|
| **Differential equivalence** — after any sequence of edits, the incrementally-maintained tree is structurally identical to a from-scratch parse of the same text. Reuse never changes results. | `incremental_tree_matches_cold_parse` |
| **Edit-path convergence** — settling after every edit, batching all edits into one run, and a cold parse all reach the same tree. | `edit_paths_converge` |
| Random edit sequences (insert / delete / replace, boundary-clamped offsets) leave the tree well-formed and inside the source. | `random_edits_keep_the_tree_consistent` |

Structural comparison ignores arena `NodeId`s and revisions: those are
implementation details that legitimately differ between an incremental
tree and a cold tree. Structure — spans, contexts, statuses, parent
shape — must not differ.

## Resource bounds

| Invariant | Enforced by |
|---|---|
| Round-budget exhaustion is a safe terminal state: the tree stays coherent, the report says so (`reached_fixpoint == false`, not cancelled), and the session is reusable. | `budget_exhaustion_leaves_a_coherent_reusable_session` |
| Cancellation mid-batch ends the run cleanly: coherent tree, `cancelled` set, and after `reset()` the session finishes the work to fixpoint. | `cancellation_mid_run_keeps_the_tree_coherent_and_session_usable` |

## Pass contract, precisely

* Children must be **contained** in the region they were parsed from.
* Children must carry their parent's **source revision**.
* Under `EngineConfig::enforce_shrink`, children must be strictly smaller
  than their parent. Without it, a child may cover its parent exactly.
* **Sibling overlap is permitted.** The contract is per-child
  containment, not mutual exclusion. Locked in by
  `overlapping_siblings_are_allowed_but_contained`; tightening this is a
  deliberate API decision, not a bug fix.
* **A panicking pass unwinds through `run()`.** The engine does not catch
  panics; a session is not reused afterwards. Locked in by
  `a_panicking_pass_aborts_the_run`.
* Every node is processed at most once per scheduled pass; runs are
  bounded by `max_rounds` (default: the schedule length). Recursion is
  expressed as one round per depth level.

## Executors

| Invariant | Enforced by |
|---|---|
| `RayonExecutor` (the `parallel` feature) produces trees structurally identical to `SerialExecutor` — merge order is deterministic. | `rayon_executor_matches_serial_executor` (runs under `--all-features`) |

## Known limits (documented, not bugs)

* Detached nodes stay in the arena (marked dead); the `Vec` never
  compacts. Sessions are meant to live as long as their document; very
  long-lived sessions over wildly churned documents may grow the arena.
  Measure before optimizing — the soak-test tier of the battle-testing
  plan exists to quantify this.
* Rounds that run out of budget leave regions pending. Consumers see
  `reached_fixpoint == false` and can re-run.
