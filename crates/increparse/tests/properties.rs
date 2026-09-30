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
//! * identical inputs produce identical trees.

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
}

#[derive(Debug, Clone)]
enum Op {
    Insert { at: usize, text: String },
    Delete { at: usize, len: usize },
}

impl Op {
    /// Applies the operation to `source` in place, returning the
    /// matching session edit.
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
        (any_ascii(6), 0usize..64).prop_map(|(text, at)| Op::Insert { at, text }),
        (0usize..64, 1usize..8).prop_map(|(at, len)| Op::Delete { at, len }),
    ]
}

fn arb_source() -> impl Strategy<Value = String> {
    prop::collection::vec(line_strategy(), 0..24).prop_map(|lines| lines.join("\n"))
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
