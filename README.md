# increparse

Multi-pass **fixpoint parsing** for editors, LSPs, and compilers.

This repository is a cargo workspace with five crates:

| Crate | Role |
|-------|------|
| [`crates/increparse`](crates/increparse) | The engine: passes, schedules, fixpoint rounds, incremental edits, executors, cancellation. Zero required dependencies. |
| [`crates/increparse-lsp`](crates/increparse-lsp) | LSP adapter: `serve()` + `Language` trait skeleton, position encodings, `Document` change translation, diagnostics bridge. |
| [`crates/increparse-nom`](crates/increparse-nom) | Wrap nom 8 parsers in passes — correct absolute-span rebasing included. |
| [`crates/increparse-chumsky`](crates/increparse-chumsky) | Wrap chumsky 0.10 parsers in passes — same rebasing, `SimpleSpan` in. |
| [`crates/increparse-lua`](crates/increparse-lua) | Define a whole language server in one Lua file (`increparse-lua-server lang.lua`) — for Neovim/VS Code users who'd rather not write Rust. |

`increparse` is *not* another parser combinator library. It is the missing
piece **around** them: an engine that executes **schedules of passes** over a
growing parse tree until the tree settles — and a pass can wrap *any* parsing
technique (`nom`, `chumsky`, a PEG, regexes, or hand-rolled scanning).

## The idea

Parsing a real program in one monolithic sweep is brittle: one syntax error in
a function body can hide the structure of an entire file. `increparse` lets
you parse in widening rounds of understanding instead.

Picture a small language with definitions like:

```text
def func_name(params) { ... }
```

- **Round 0** runs a cheap, error-tolerant pass over the whole file that
  matches only the skeleton `def func_name(params) { * }` — the `*` is an
  explicit *hole*: a child region the pass did not parse, pushed onto the tree
  with its own context (`function_name`, `params`).
- **Round 1** runs a deeper pass over each hole, with that context threaded
  in, expanding statements inside bodies.
- Later rounds keep expanding until every region is either accepted
  (`Done`) or permanently failed.

Because failed regions stay in the tree as leaves, consumers — say, an LSP
answering "what functions does this file define?" — still see the coarse
structure even while deep passes are failing. That is the error-resilience
payoff, and it is why the design suits language servers.

## How a run works

Each node in the [`ParseTree`] holds a [`Span`] into the source, a context
value, and a [`Status`]:

```text
Unparsed ──▶ Expanded ──┐   (children carry the remaining work)
    │                   │
    ├──▶ Done           │   (region accepted, no children)
    └──▶ Failed ──┐     │
                  │     │
   retried by the next pass in the schedule; once passes run
   out, the failure is permanent and the node stays as a leaf
```

- **Round `r`** applies `schedule[r]` to every node ready for it (nodes
  created or failed in round `r-1` are ready for round `r`).
- A run reaches its **fixpoint** when a round finds no ready nodes — or hits
  the round cap (one round per scheduled pass, by default).
- The engine merges batch results in job order, so runs are **deterministic**
  regardless of the executor.

### Termination by construction

Passes may only produce child spans **contained in** their parent and on the
same source revision. The engine rejects any outcome that violates this,
marking the node failed. Since round `r` only processes nodes at depth `r`
and runs are capped at one round per scheduled pass, every node is processed
at most once per pass and no schedule can loop forever. (For "always
divides" passes, `EngineConfig::enforce_shrink` additionally requires
children to be strictly smaller; by default a child may cover its parent
exactly — e.g. a file containing exactly one function.)

## Getting started

- **New to increparse?** [`doc/parser-quickstart.md`](doc/parser-quickstart.md)
  builds a working INI parser in ~30 minutes — no editor, no LSP — in Rust
  or, if you'd rather not write Rust at all, in pure Lua.
- **Then** [`doc/lsp-tutorial.md`](doc/lsp-tutorial.md) puts it into VS Code
  and Neovim as a real language server.

## Quick start

```rust
use increparse::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ctx {
    File,
    Function { name: String },
}

// Round 0: a real pass would scan for `def name(...) { ... }` skeletons and
// expand each one into a child region. Here we fake one function. Passes
// are plain closures — or implement `Pass` for a struct if you prefer.
let functions = pass_fn(|_source: &str, span, ctx: &Ctx| match ctx {
    Ctx::File => Outcome::one(
        Span::new(span.start + 4, span.end, span.rev),
        Ctx::Function { name: "main".into() },
    ),
    Ctx::Function { .. } => Outcome::Done,
});

// Round 0 finds the function; round 1 settles what round 0 created.
let engine = Engine::with((functions, functions));

let source = "def main() { }";
let mut tree = ParseTree::from_source(source, 0, Ctx::File);

let report = engine.run(source, &mut tree, &SerialExecutor, &increparse::CancelToken::new());

assert!(report.reached_fixpoint);
assert_eq!(tree.status(tree.root()), Status::Expanded);
assert_eq!(tree.ctx(tree.children(tree.root())[0]), &Ctx::Function { name: "main".into() });
```

A fuller, runnable version — `def name(params) { return expr; }` parsed in
three passes, with a malformed definition and an empty return surviving in the
tree — lives in `examples/mini_lang.rs`:

```sh
cargo run --example mini_lang
```

## Incremental edits

The tree is built to be *re-parsed*, not rebuilt. A [`Session`] wraps a
[`ParseTree`] that can absorb [`Edit`]s: every span is remapped into the new
coordinates and only the nodes the edit touched are reset for re-parsing.
When the next run re-expands their parents, produced children are matched
against the surviving ones **by span and context** — equal children keep
their identity, their status, and their whole subtree:

```rust,ignore
session.edit(Edit::insert(source.len(), appended.len()));
let report = session.run(&engine, &source, &SerialExecutor, &CancelToken::new());
// report.nodes_processed == 3  — root scan + the new function's chain only;
// every pre-existing function kept its NodeId and parsed subtree.
```

An edit inside one function body re-parses that function; every other
function is carried over untouched. Fixing a syntax error heals the region
in place — the node keeps its identity and retries every pass, including
ones it had previously exhausted. See `examples/mini_lang.rs` for a full
walkthrough (append a function, then fix a broken return — 3 nodes re-parsed
per edit instead of the whole file).

## Language servers

`increparse-lsp` bridges the engine to the Language Server Protocol. Two
layers:

- **`serve()` + `SimpleLanguage`** — describe the language with a builder
  (engine, root context, diagnostics hook, optional `label_fn`/`symbols_fn`
  for the outline) and get a complete server: initialize, document
  bookkeeping, incremental change translation, diagnostics publishing,
  symbol dispatch, folding ranges, document highlight, and a structurally
  deadlock-free shutdown. Runs on stdio via `lsp-server`; `serve_on()`
  accepts any connection you own. Power users can implement the `Language`
  trait directly.
- **Framework-agnostic pieces** — `LineIndex`/`PositionEncoding` (byte ↔
  UTF-8/16/32 positions), `Document<C>` (didChange events → byte `Edit`s →
  one engine run, with tree reuse), `diagnostics()`, and `BackgroundRunner`
  (parse on a worker thread with coalescing and cancellation) — for when
  you'd rather write the loop yourself (or use another server framework;
  only the `serve()` layer needs `lsp-server`).

`crates/increparse-lsp/examples/mini_lang_server.rs` is a complete small
server (diagnostics + document symbols) with an end-to-end stdio smoke test
in `crates/increparse-lsp/tests/server_smoke.rs`.

**Want to build your own?** Follow [`doc/lsp-tutorial.md`](doc/lsp-tutorial.md)
— a step-by-step guide that turns MiniLang into a working language server
and runs it in VS Code and Neovim.

## Compiler design

memjs ([`crates/memjs`](crates/memjs)) is the compiler-design pilot: a
JavaScript subset — closures, arrows, objects with shorthand, `var` /
`let` / `const` with real scoping and hoisting semantics,
`for`/`for..of`/`for..in`, arrays with `map`/`filter`, truthiness,
`==` and `===` — built directly on the engine. One pass segments a file into top-level items and parses each
item where it stands; the ASTs ride in the tree as contexts; a
content-keyed cache keeps untouched items pointer-identical across
edits, so an edit to one function re-parses only that function. A
tree-walking interpreter executes the settled tree, and a transpiler
prints the same AST back to JavaScript — every memjs program is checked
against Node in the test suite, byte for byte.

It is also the working sketch of the two patterns a real compiler
front-end needs here: parse inside the pass, analyze outside it (scope
and cross-function checks are consumer walks over the settled tree,
since pass context flows down but not up), and carry language payloads
in the context (`Ctx::Fn(Arc<FnDef>)`). And the pilot already carries the
gradual-memory thesis: `// @own` comments route arrays and objects
through per-activation arenas — dropped deterministically on return,
never on the reference-counted heap — with enforced moves and read-only
`// @ref` borrows, while annotated programs stay 100% valid JavaScript
(the Node differential covers them). The roadmap: annotation inference
— most code needs no annotations at all.

## Executors and cancellation

A round's batch of nodes goes through the [`Executor`] trait:

- [`SerialExecutor`] — jobs run one at a time on the calling thread (default).
- [`RayonExecutor`] — jobs run on a rayon thread pool; enable the `parallel`
  feature. Useful for batch compilation; results still merge in job order.

For LSP-style interactive use, [`BackgroundRunner`] is the ready-made
version of this pattern: it owns a worker thread and a `Session`, coalesces
bursts of submissions into the newest, reuses everything an edit did not
touch, and cancels in-flight runs on request (the engine checks at job
granularity). The responsiveness comes from cancellation *between batches*,
not from intra-round parallelism — which is why the default build has no
concurrency machinery at all.

## API tour

| Piece | Role |
|-------|------|
| [`Pass`] | Your parse logic: `(&str, Span, &Ctx) -> Outcome<Ctx>`. |
| [`Schedule`] | Ordered passes; round `r` uses pass `r`. |
| [`ParseTree`] | Arena of `(span, ctx, status)` nodes with stable [`NodeId`]s. |
| [`Engine`] | Drives rounds to a fixpoint; rejects contract-violating child spans. |
| [`Session`] | Long-lived document: edits + re-parses with subtree reuse. |
| [`Edit`] | One text change: `replace(start, old_end, new_end)`. |
| [`Outcome`] | `Expand(children)` / `Done` / `Failed`. |
| [`RunReport`] | Rounds run, work done, `Violation`s, fixpoint/cancellation flags. |
| [`Span`] | Byte range + revision into the source. |
| [`Status`] / `StatusCounts` | Lifecycle state of a node; per-status totals. |
| [`NodeId`] | Stable node handle. |
| [`Executor`] / [`CancelToken`] | Runs a round's batch; cooperative cancellation. |
| [`BackgroundRunner`] | Parse on a worker thread: coalescing, session reuse, cancellation. |

## Roadmap

- Finer-grained reuse hooks (e.g. matching by user-supplied keys instead of
  `PartialEq`).

## Status

v0.1.0 — core semantics are settling; the API may still change.

[`Pass`]: https://docs.rs/increparse/latest/increparse/trait.Pass.html
[`Schedule`]: https://docs.rs/increparse/latest/increparse/struct.Schedule.html
[`ParseTree`]: https://docs.rs/increparse/latest/increparse/struct.ParseTree.html
[`Engine`]: https://docs.rs/increparse/latest/increparse/struct.Engine.html
[`Session`]: https://docs.rs/increparse/latest/increparse/struct.Session.html
[`Edit`]: https://docs.rs/increparse/latest/increparse/struct.Edit.html
[`Outcome`]: https://docs.rs/increparse/latest/increparse/enum.Outcome.html
[`RunReport`]: https://docs.rs/increparse/latest/increparse/struct.RunReport.html
[`Span`]: https://docs.rs/increparse/latest/increparse/struct.Span.html
[`Status`]: https://docs.rs/increparse/latest/increparse/enum.Status.html
[`NodeId`]: https://docs.rs/increparse/latest/increparse/struct.NodeId.html
[`Executor`]: https://docs.rs/increparse/latest/increparse/trait.Executor.html
[`SerialExecutor`]: https://docs.rs/increparse/latest/increparse/struct.SerialExecutor.html
[`RayonExecutor`]: https://docs.rs/increparse/latest/increparse/struct.RayonExecutor.html
[`CancelToken`]: https://docs.rs/increparse/latest/increparse/struct.CancelToken.html
[`BackgroundRunner`]: https://docs.rs/increparse-lsp/latest/increparse_lsp/background/struct.BackgroundRunner.html
[`LineIndex`]: https://docs.rs/increparse-lsp/latest/increparse_lsp/struct.LineIndex.html
[`PositionEncoding`]: https://docs.rs/increparse-lsp/latest/increparse_lsp/enum.PositionEncoding.html
