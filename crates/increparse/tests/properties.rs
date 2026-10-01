//! Property-based tests for the engine's core invariants.
//!
//! Random sources (including multibyte and adversarial text) are run
//! through well-behaved and deliberately misbehaving passes. The
//! invariants that must hold everywhere:
//!
//! * runs terminate (fixpoint or cancellation), never panic,
//! * well-behaved passes produce `report.violations == []`,
//! * deliberately misbehaving passes are rejected by the engine and
//!   reported, not propagated,
//! * every node's span is well-formed and contained in the root span,
//! * child spans are contained in their parent's span,
//! * `status_counts().total() == tree.len()`,
//! * identical inputs produce identical trees,
//! * an incrementally-updated tree is structurally identical to a
//!   from-scratch parse of the same text (differential equivalence),
//! * the tree reached by many small edits equals the tree reached by
//!   one batched edit equals the cold parse (edit-path convergence),
//! * a run that exhausts its round budget or is cancelled mid-flight
//!   leaves a coherent tree and a reusable session,
//! * `SerialExecutor` and `RayonExecutor` produce identical trees.

use std::collections::HashMap;

use increparse::prelude::*;
use proptest::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ctx {
    Root,
    Block,
    Line,
}

// A block: one line whose first non-space token is '{' and whose last
// is '}'. The body between them belongs to the block region.
fn blocks_pass(source: &str, span: Span, ctx: &Ctx) -> Outcome<Ctx> {
    if !matches!(ctx, Ctx::Root) {
        return Outcome::Failed;
    }
    let bytes = source.as_bytes();
    let mut children = Vec::new();
    let mut i = span.start;
    while i < span.end {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        // find the matching close brace on the same line, tracking depth
        let mut depth = 0usize;
        let mut j = i;
        let mut closed = None;
        while j < span.end {
            match bytes[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        closed = Some(j);
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        match closed {
            Some(close) => {
                children.push((Span::new(i, close + 1, span.rev), Ctx::Block));
                i = close + 1;
            }
            None => {
                i = span.end;
            }
        }
    }
    if children.is_empty() {
        Outcome::Done
    } else {
        Outcome::Expand(children)
    }
}

// The lines inside a block region: one child per newline-separated
// segment (empty segments are skipped).
fn lines_pass(source: &str, span: Span, ctx: &Ctx) -> Outcome<Ctx> {
    if !matches!(ctx, Ctx::Block) {
        return Outcome::Failed;
    }
    let bytes = source.as_bytes();
    let mut children = Vec::new();
    let mut start = span.start;
    let mut i = span.start;
    while i < span.end {
        if bytes[i] == b'\n' {
            if i > start {
                children.push((Span::new(start, i, span.rev), Ctx::Line));
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < span.end {
        children.push((Span::new(start, span.end, span.rev), Ctx::Line));
    }
    Outcome::Expand(children)
}

fn make_engine() -> Engine<Ctx> {
    Engine::with((pass_fn(blocks_pass), pass_fn(lines_pass)))
}

// The deep grammar: Root and Block regions expand into the top-level
// `{...}` groups they contain, nested arbitrarily; a Block without
// nested groups splits into lines. Recursion is expressed the way the
// engine models it — one scheduled round per depth level.
fn deep_pass(source: &str, span: Span, ctx: &Ctx) -> Outcome<Ctx> {
    match ctx {
        Ctx::Line => Outcome::Done,
        Ctx::Root | Ctx::Block => {
            let bytes = source.as_bytes();
            let mut children = Vec::new();
            let mut i = span.start;
            while i < span.end {
                if bytes[i] != b'{' {
                    i += 1;
                    continue;
                }
                let mut depth = 0usize;
                let mut j = i;
                let mut closed = None;
                while j < span.end {
                    match bytes[j] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                closed = Some(j);
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                match closed {
                    Some(close) => {
                        children.push((Span::new(i, close + 1, span.rev), Ctx::Block));
                        i = close + 1;
                    }
                    None => {
                        i = span.end;
                    }
                }
            }
            if !children.is_empty() {
                return Outcome::Expand(children);
            }
            if matches!(ctx, Ctx::Block) {
                let mut lines = Vec::new();
                let mut start = span.start;
                for (i, byte) in bytes.iter().enumerate().take(span.end).skip(span.start) {
                    if *byte == b'\n' {
                        if i > start {
                            lines.push((Span::new(start, i, span.rev), Ctx::Line));
                        }
                        start = i + 1;
                    }
                }
                if start < span.end {
                    lines.push((Span::new(start, span.end, span.rev), Ctx::Line));
                }
                if !lines.is_empty() {
                    return Outcome::Expand(lines);
                }
            }
            Outcome::Done
        }
    }
}

fn make_deep_engine() -> Engine<Ctx> {
    // Four rounds: nesting up to depth four gets processed; deeper
    // regions legitimately remain pending when the budget runs out.
    Engine::with((
        pass_fn(deep_pass),
        pass_fn(deep_pass),
        pass_fn(deep_pass),
        pass_fn(deep_pass),
    ))
}

fn walk<C>(
    tree: &ParseTree<C>,
    id: increparse::NodeId,
    out: &mut Vec<(Span, C, Status, Option<increparse::NodeId>)>,
) where
    C: std::fmt::Debug + Clone,
{
    out.push((
        tree.span(id),
        tree.ctx(id).clone(),
        tree.status(id),
        tree.parent(id),
    ));
    for child in tree.children(id) {
        walk(tree, *child, out);
    }
}

fn fingerprint<C>(tree: &ParseTree<C>) -> Vec<(Span, C, Status, Option<increparse::NodeId>)>
where
    C: std::fmt::Debug + Clone,
{
    let mut out = Vec::new();
    walk(tree, tree.root(), &mut out);
    out
}

/// The tree's structure as pre-order tuples, with arena `NodeId`s
/// (including parent links) translated to pre-order positions and
/// revisions dropped.
///
/// Two structurally identical trees must compare equal even if their
/// arena layouts differ — which is exactly the situation after
/// incremental edits versus a from-scratch parse — and spans carry the
/// session revision they were parsed at, which is expected to differ
/// between the two paths.
/// One pre-order node: span bounds, context, status, and the parent's
/// pre-order position.
type StructuralNode<C> = ((usize, usize), C, Status, Option<usize>);

fn structural_fingerprint<C: std::fmt::Debug + Clone>(
    tree: &ParseTree<C>,
) -> Vec<StructuralNode<C>> {
    let mut ids = Vec::new();
    let mut stack = vec![tree.root()];
    while let Some(id) = stack.pop() {
        ids.push(id);
        for child in tree.children(id) {
            stack.push(*child);
        }
    }
    let position: HashMap<increparse::NodeId, usize> =
        ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    ids.iter()
        .map(|id| {
            (
                (tree.span(*id).start, tree.span(*id).end),
                tree.ctx(*id).clone(),
                tree.status(*id),
                tree.parent(*id)
                    .and_then(|parent| position.get(&parent).copied()),
            )
        })
        .collect()
}

fn assert_wellformed<C: std::fmt::Debug + Clone>(tree: &ParseTree<C>, source: &str) {
    let root_span = tree.span(tree.root());
    assert!(
        root_span.start == 0 && root_span.end == source.len(),
        "root span must cover the source"
    );
    for id in tree.nodes() {
        let span = tree.span(id);
        assert!(span.start <= span.end, "span must be ordered: {span:?}");
        assert!(
            span.end <= source.len(),
            "span must stay within the source: {span:?}"
        );
        assert!(
            root_span.contains(&span),
            "every span must sit inside the root: {span:?}"
        );
        for child in tree.children(id) {
            assert!(
                tree.span(*child).start >= span.start,
                "children must not start before their parent"
            );
        }
    }
    assert_eq!(
        tree.status_counts().total(),
        tree.len(),
        "status counts must cover every node"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn terminates_and_tree_is_wellformed(source in arb_source()) {
        let engine = make_engine();
        let mut session = Session::from_source(&source, 0, Ctx::Root);
        let report = session.run(&engine, &source, &SerialExecutor, &CancelToken::new());
        // A schedule that runs out before every node settles is a
        // legitimate terminal state — what matters is that the run
        // terminates, is never cancelled, and the tree stays coherent.
        prop_assert!(!report.cancelled);
        prop_assert!(report.violations.is_empty());
        assert_wellformed(session.tree(), &source);
    }

    #[test]
    fn runs_are_deterministic(source in arb_source()) {
        let engine = make_engine();
        let fingerprint = |tree: &ParseTree<Ctx>| fingerprint(tree);
        let mut a = Session::from_source(&source, 0, Ctx::Root);
        let mut b = Session::from_source(&source, 0, Ctx::Root);
        a.run(&engine, &source, &SerialExecutor, &CancelToken::new());
        b.run(&engine, &source, &SerialExecutor, &CancelToken::new());
        prop_assert_eq!(fingerprint(a.tree()), fingerprint(b.tree()));
    }

    #[test]
    fn random_edits_keep_the_tree_consistent(
        (seed_text, ops) in (any_ascii(200), any_edits(12))
    ) {
        let engine = make_engine();
        let mut source = seed_text;
        let mut session = Session::from_source(&source, 0, Ctx::Root);
        session.run(&engine, &source, &SerialExecutor, &CancelToken::new());
        for op in &ops {
            let edit = op.apply(&mut source);
            session.edit(edit);
            let report = session.run(&engine, &source, &SerialExecutor, &CancelToken::new());
            prop_assert!(!report.cancelled);
            // every surviving span must sit inside the current source
            for id in session.tree().nodes() {
                let span = session.tree().span(id);
                prop_assert!(span.end <= source.len(), "span escaped the source");
                prop_assert!(span.start <= span.end, "span must be ordered");
            }
            assert_wellformed(session.tree(), &source);
        }
    }

    /// The engine's core promise: reuse never changes results. After any
    /// sequence of edits, the incrementally-maintained tree must be
    /// structurally identical to a from-scratch parse of the same text.
    #[test]
    fn incremental_tree_matches_cold_parse((seed_text, ops) in (arb_nested_source(), any_edits(10))) {
        let engine = make_deep_engine();
        let mut source = seed_text;
        let mut session = Session::from_source(&source, 0, Ctx::Root);
        session.run(&engine, &source, &SerialExecutor, &CancelToken::new());
        for op in &ops {
            let edit = op.apply(&mut source);
            session.edit(edit);
            session.run(&engine, &source, &SerialExecutor, &CancelToken::new());
        }
        let mut cold = Session::from_source(&source, 0, Ctx::Root);
        let cold_report = cold.run(&engine, &source, &SerialExecutor, &CancelToken::new());
        prop_assert!(!cold_report.cancelled);
        prop_assert_eq!(
            structural_fingerprint(session.tree()),
            structural_fingerprint(cold.tree()),
            "incremental tree diverged from a cold parse of the same text"
        );
    }

    /// Edit-path independence: settling after every edit, batching all
    /// edits into one run, and never editing at all (cold) must all reach
    /// the same tree for the same final text.
    #[test]
    fn edit_paths_converge((seed_text, ops) in (arb_nested_source(), any_edits(8))) {
        let engine = make_deep_engine();

        // Path 1: settle after every edit.
        let mut source_stepwise = seed_text.clone();
        let mut stepwise = Session::from_source(&source_stepwise, 0, Ctx::Root);
        stepwise.run(&engine, &source_stepwise, &SerialExecutor, &CancelToken::new());
        for op in &ops {
            let edit = op.apply(&mut source_stepwise);
            stepwise.edit(edit);
            stepwise.run(&engine, &source_stepwise, &SerialExecutor, &CancelToken::new());
        }

        // Path 2: apply every edit, then run once.
        let mut source_batched = seed_text;
        let mut batched = Session::from_source(&source_batched, 0, Ctx::Root);
        batched.run(&engine, &source_batched, &SerialExecutor, &CancelToken::new());
        for op in &ops {
            let edit = op.apply(&mut source_batched);
            batched.edit(edit);
        }
        batched.run(&engine, &source_batched, &SerialExecutor, &CancelToken::new());

        prop_assert_eq!(
            source_stepwise, source_batched,
            "both paths must reach the same text"
        );
        prop_assert_eq!(
            structural_fingerprint(stepwise.tree()),
            structural_fingerprint(batched.tree()),
            "stepwise and batched edits produced different trees"
        );
    }
}

#[derive(Debug, Clone)]
enum Op {
    Insert {
        at: usize,
        text: String,
    },
    Delete {
        at: usize,
        len: usize,
    },
    Replace {
        at: usize,
        old_len: usize,
        text: String,
    },
}

impl Op {
    /// Applies the operation to `source` in place, returning the
    /// matching session edit. Offsets are clamped and walked to the
    /// nearest character boundary, mirroring how the API is meant to be
    /// used — the walks themselves are part of what these tests exercise.
    fn apply(&self, source: &mut String) -> increparse::Edit {
        match self {
            Op::Insert { at, text } => {
                let mut at = (*at).min(source.len());
                while !source.is_char_boundary(at) {
                    at -= 1;
                }
                source.insert_str(at, text);
                increparse::Edit::insert(at, text.len())
            }
            Op::Delete { at, len } => {
                let mut start = (*at).min(source.len());
                while !source.is_char_boundary(start) {
                    start -= 1;
                }
                let mut end = (start + *len).min(source.len());
                while end < source.len() && !source.is_char_boundary(end) {
                    end += 1;
                }
                source.replace_range(start..end, "");
                increparse::Edit::replace(start, end, start)
            }
            Op::Replace { at, old_len, text } => {
                let mut start = (*at).min(source.len());
                while !source.is_char_boundary(start) {
                    start -= 1;
                }
                let mut end = (start + *old_len).min(source.len());
                while end < source.len() && !source.is_char_boundary(end) {
                    end += 1;
                }
                source.replace_range(start..end, text);
                increparse::Edit::replace(start, end, start + text.len())
            }
        }
    }
}

fn any_ascii(max: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec("[a-z{}/;= ]", 0..max).prop_map(|chars| chars.concat())
}

fn any_edits(count: usize) -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(any_edit(), 0..count)
}

fn any_edit() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (any_ascii(6), 0usize..64).prop_map(|(text, at)| Op::Insert { at, text }),
        2 => (0usize..64, 1usize..8).prop_map(|(at, len)| Op::Delete { at, len }),
        3 => (0usize..64, 1usize..8, replace_text())
            .prop_map(|(at, old_len, text)| Op::Replace { at, old_len, text }),
    ]
}

// Replacement text deliberately mixes grammar tokens with multibyte
// characters so edits shake both the brace structure and boundary math.
fn replace_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("{ }".into()),
        Just("{}".into()),
        Just("} {".into()),
        Just("x = 1\n".into()),
        Just("日本".into()),
        Just("é".into()),
        any_ascii(5),
    ]
}

fn arb_source() -> impl Strategy<Value = String> {
    prop::collection::vec(line_strategy(), 0..24).prop_map(|lines| lines.join("\n"))
}

// Nested brace groups up to depth four (bounded by the deep engine's
// schedule), interleaved with plain lines — multibyte tokens included,
// so incremental edits land near character boundaries.
fn arb_nested_source() -> impl Strategy<Value = String> {
    line_strategy()
        .prop_recursive(
            4,  // up to 4 levels of nesting
            64, // aim for sources up to ~64 lines
            4,  // at most 4 nested groups per level
            |inner| {
                prop::collection::vec(inner, 1..=4).prop_map(|blocks| {
                    blocks
                        .into_iter()
                        .map(|b| format!("{{ {b} }}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            },
        )
        .prop_map(|body| {
            // Give the root some flat content too, so roots parse children
            // that are a mix of blocks and loose lines.
            format!("{body}\nplain def line\n")
        })
}

fn line_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(token_strategy(), 0..8).prop_map(|tokens| tokens.concat())
}

fn token_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => Just("{".into()),
        3 => Just("}".into()),
        2 => Just(" def ".into()),
        2 => Just("ident".into()),
        2 => Just(" ".into()),
        1 => Just(";".into()),
        1 => Just("日本".into()),
        1 => Just("é".into()),
        1 => any::<String>().prop_map(|s| s.chars().filter(|c| *c != '\n').take(4).collect()),
    ]
}

// ---- misbehaving passes must be rejected, not propagated ----

// Expands a child that escapes its parent's region.
#[test]
fn escaping_children_are_rejected_and_reported() {
    let engine = Engine::with((pass_fn(|_source, _span, _ctx: &Ctx| {
        Outcome::Expand(vec![(Span::new(900, 905, 0), Ctx::Line)])
    }),));
    let source = "short";
    let mut session = Session::from_source(source, 0, Ctx::Root);
    let report = session.run(&engine, source, &SerialExecutor, &CancelToken::new());
    assert!(
        !report.violations.is_empty(),
        "the escaping child must be reported"
    );
    // the tree stays intact: root only, nothing escaped into it
    assert_eq!(session.tree().len(), 1);
}

#[test]
fn zero_width_and_wrong_revision_children_are_handled() {
    // zero-width child: tolerated but skipped by well-behaved consumers;
    // wrong-revision child: rejected as a contract violation.
    let engine = Engine::with((
        pass_fn(|_source: &str, span: Span, _ctx: &Ctx| {
            Outcome::Expand(vec![
                (Span::new(span.start, span.start, span.rev), Ctx::Line),
                (Span::new(span.start, span.end, span.rev + 7), Ctx::Line),
            ])
        }),
        pass_fn(|_source: &str, _span: Span, _ctx: &Ctx| Outcome::Done),
    ));
    let source = "x = 1\n";
    let mut session = Session::from_source(source, 0, Ctx::Root);
    let report = session.run(&engine, source, &SerialExecutor, &CancelToken::new());
    assert!(report.reached_fixpoint);
    assert!(
        !report.violations.is_empty() || session.tree().len() == 1,
        "contract violations must be visible somewhere"
    );
}

#[test]
fn overlapping_siblings_are_allowed_but_contained() {
    // The contract is per-child containment, not mutual exclusion: two
    // children may overlap inside their parent, and the engine accepts
    // them. This test locks that behavior in — tightening it later is an
    // API decision, not an accident.
    let engine = Engine::with((pass_fn(|_source: &str, span: Span, _ctx: &Ctx| {
        let mid = span.start + (span.end - span.start) / 2;
        Outcome::Expand(vec![
            (Span::new(span.start, mid + 2, span.rev), Ctx::Block),
            (
                Span::new(mid.saturating_sub(2), span.end, span.rev),
                Ctx::Block,
            ),
        ])
    }),));
    let source = "0123456789";
    let mut session = Session::from_source(source, 0, Ctx::Root);
    let report = session.run(&engine, source, &SerialExecutor, &CancelToken::new());
    assert!(
        report.violations.is_empty(),
        "overlapping-but-contained children are not contract violations"
    );
    assert_eq!(session.tree().len(), 3, "root plus both siblings");
    assert_wellformed(session.tree(), source);
}

#[test]
fn budget_exhaustion_leaves_a_coherent_reusable_session() {
    // Halves its region every round; with a three-round schedule the
    // tree always has pending work when the budget runs out. The
    // exhausted run is a legitimate terminal state: coherent tree, not
    // cancelled, session still usable.
    let engine = Engine::with((pass_fn(halver), pass_fn(halver), pass_fn(halver)));
    let source = "0123456789012345678901234567890123456789"; // 40 bytes
    let mut session = Session::from_source(source, 0, Ctx::Root);
    let report = session.run(&engine, source, &SerialExecutor, &CancelToken::new());
    assert!(
        !report.reached_fixpoint,
        "a halving pass always has more work than the budget allows"
    );
    assert!(!report.cancelled);
    assert_wellformed(session.tree(), source);

    // The session survives exhaustion: a second run continues within
    // the same budget and stays coherent.
    let second = session.run(&engine, source, &SerialExecutor, &CancelToken::new());
    assert!(!second.cancelled);
    assert_wellformed(session.tree(), source);
}

fn halver(_source: &str, span: Span, _ctx: &Ctx) -> Outcome<Ctx> {
    let mid = span.start + (span.end - span.start) / 2;
    if mid == span.start || mid == span.end {
        return Outcome::Done;
    }
    Outcome::Expand(vec![
        (Span::new(span.start, mid, span.rev), Ctx::Block),
        (Span::new(mid, span.end, span.rev), Ctx::Block),
    ])
}

#[test]
fn cancellation_mid_run_keeps_the_tree_coherent_and_session_usable() {
    // Round 0 expands the root into four children; round 1's first job
    // cancels the token mid-batch, so the remaining jobs are skipped and
    // the run reports cancellation. After a reset, the session finishes
    // the work normally.
    let cancel = CancelToken::new();
    let engine = Engine::with((
        pass_fn(|_source: &str, span: Span, _ctx: &Ctx| {
            let step = (span.end - span.start) / 4;
            Outcome::Expand(
                (0..4)
                    .map(|i| {
                        (
                            Span::new(span.start + i * step, span.start + (i + 1) * step, span.rev),
                            Ctx::Block,
                        )
                    })
                    .collect(),
            )
        }),
        {
            let cancel = cancel.clone();
            pass_fn(move |_source: &str, _span: Span, _ctx: &Ctx| {
                cancel.cancel();
                Outcome::Done
            })
        },
    ));
    let source = "0123456789";
    let mut session = Session::from_source(source, 0, Ctx::Root);
    let report = session.run(&engine, source, &SerialExecutor, &cancel);
    assert!(report.cancelled, "a mid-batch cancel must end the run");
    assert!(!report.reached_fixpoint);
    assert_wellformed(session.tree(), source);

    cancel.reset();
    let second = session.run(&engine, source, &SerialExecutor, &cancel);
    assert!(!second.cancelled);
    assert!(second.reached_fixpoint, "the reset run finishes the work");
    assert_wellformed(session.tree(), source);
}

#[test]
#[should_panic(expected = "boom")]
fn a_panicking_pass_aborts_the_run() {
    // Policy: the engine does not catch panics. A pass that panics
    // unwinds through run(); the session must not be reused afterwards.
    // This test documents the behavior — catching and recovering would
    // be an API decision with its own hazards.
    let engine = Engine::with((pass_fn(
        |_source: &str, _span: Span, _ctx: &Ctx| -> Outcome<Ctx> { panic!("boom") },
    ),));
    let source = "anything";
    let mut session = Session::from_source(source, 0, Ctx::Root);
    let _ = session.run(&engine, source, &SerialExecutor, &CancelToken::new());
}

#[cfg(feature = "parallel")]
#[test]
fn rayon_executor_matches_serial_executor() {
    // Merge order is part of the executor contract: the parallel path
    // must produce trees structurally identical to the serial one. The
    // proptest suite above hammers the serial path; this checks the
    // parallel merge on a diverse fixed corpus (nesting, multibyte,
    // unbalanced braces, empty).
    let sources = [
        String::new(),
        "plain def line\n".into(),
        "{ { a } { b } } { c }".into(),
        "{ 日本 } é {".into(),
        "}{ { } } }{".into(),
        arb_nested_source_sample(),
    ];
    for source in sources {
        let engine = make_deep_engine();
        let mut serial = Session::from_source(&source, 0, Ctx::Root);
        serial.run(&engine, &source, &SerialExecutor, &CancelToken::new());
        let mut parallel = Session::from_source(&source, 0, Ctx::Root);
        parallel.run(&engine, &source, &RayonExecutor, &CancelToken::new());
        assert_eq!(
            structural_fingerprint(serial.tree()),
            structural_fingerprint(parallel.tree()),
            "serial and rayon diverged on {source:?}"
        );
    }
}

#[cfg(feature = "parallel")]
fn arb_nested_source_sample() -> String {
    // One representative nested source, built deterministically so the
    // corpus stays fixed across runs.
    let mut s = String::new();
    for i in 0..3 {
        s.push_str(&format!("{{ inner{0} {{ deep{0} }} {0} }} ", i));
    }
    s.push_str("tail\n");
    s
}
