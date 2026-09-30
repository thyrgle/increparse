use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use increparse::{CancelToken, Engine, Outcome, Pass, Schedule, SerialExecutor, Span, Status};
use increparse_lsp::{diagnostics, DiagnosticsOptions, Document, PositionEncoding};
use lsp_types::{Diagnostic, Position, TextDocumentContentChangeEvent, Uri};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ctx {
    File,
    Left,
    Right { head: char },
}

/// File -> [Left(0..4), Right(4..len)], context embedding the split byte.
#[derive(Clone)]
struct SplitAtFour {
    calls: Arc<AtomicUsize>,
}

impl Pass for SplitAtFour {
    type Ctx = Ctx;

    fn parse(&self, source: &str, span: Span, ctx: &Ctx) -> Outcome<Ctx> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !matches!(ctx, Ctx::File) {
            return Outcome::Failed;
        }
        let split = (span.start + 4).min(span.end);
        let head = source[split..].chars().next().unwrap_or('?');
        Outcome::Expand(vec![
            (Span::new(span.start, split, span.rev), Ctx::Left),
            (Span::new(split, span.end, span.rev), Ctx::Right { head }),
        ])
    }
}

/// Left/Right -> Done.
#[derive(Clone)]
struct Settle {
    calls: Arc<AtomicUsize>,
}

impl Pass for Settle {
    type Ctx = Ctx;

    fn parse(&self, _source: &str, _span: Span, ctx: &Ctx) -> Outcome<Ctx> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match ctx {
            Ctx::File => Outcome::Failed,
            _ => Outcome::Done,
        }
    }
}

/// Fails any region containing `!`, else Done.
struct FailOnBang;

impl Pass for FailOnBang {
    type Ctx = Ctx;

    fn parse(&self, source: &str, span: Span, _ctx: &Ctx) -> Outcome<Ctx> {
        if source[span.to_range()].contains('!') {
            Outcome::Failed
        } else {
            Outcome::Done
        }
    }
}

fn make_engine() -> (Engine<Ctx>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let split_calls = Arc::new(AtomicUsize::new(0));
    let settle_calls = Arc::new(AtomicUsize::new(0));
    let mut schedule = Schedule::new();
    schedule.push(SplitAtFour {
        calls: split_calls.clone(),
    });
    schedule.push(Settle {
        calls: settle_calls.clone(),
    });
    (Engine::new(schedule), split_calls, settle_calls)
}

fn change(range: Option<(u32, u32, u32, u32)>, text: &str) -> TextDocumentContentChangeEvent {
    TextDocumentContentChangeEvent {
        range: range.map(|(sl, sc, el, ec)| lsp_types::Range {
            start: Position {
                line: sl,
                character: sc,
            },
            end: Position {
                line: el,
                character: ec,
            },
        }),
        range_length: None,
        text: text.into(),
    }
}

fn open(text: &str) -> Document<Ctx> {
    let uri: Uri = "file:///test.txt".parse().unwrap();
    Document::open(
        uri,
        0,
        "".into(),
        text.into(),
        PositionEncoding::Utf16,
        Ctx::File,
    )
}

#[test]
fn incremental_edit_sequence_translates_to_text() {
    let (engine, _split, _settle) = make_engine();
    let mut doc = open("abcdefghij");

    let report = doc.apply_changes(
        &engine,
        1,
        &[change(Some((0, 3, 0, 3)), "XY")],
        &SerialExecutor,
        &CancelToken::new(),
    );
    assert!(report.reached_fixpoint);
    assert_eq!(doc.text(), "abcXYdefghij");
    assert_eq!(doc.revision(), 1);
    assert_eq!(doc.version(), 1);

    doc.apply_changes(
        &engine,
        2,
        &[
            change(Some((0, 3, 0, 5)), ""),
            change(Some((0, 3, 0, 3)), "ZZ"),
        ],
        &SerialExecutor,
        &CancelToken::new(),
    );
    assert_eq!(doc.text(), "abcZZdefghij");
    assert_eq!(doc.revision(), 3, "one revision bump per event");
    assert_eq!(doc.version(), 2);
}

#[test]
fn full_sync_event_replaces_whole_text() {
    let (engine, _split, _settle) = make_engine();
    let mut doc = open("abcdefghij");

    doc.apply_changes(
        &engine,
        7,
        &[change(None, "short")],
        &SerialExecutor,
        &CancelToken::new(),
    );

    assert_eq!(doc.text(), "short");
    assert_eq!(doc.version(), 7);
    assert_eq!(doc.revision(), 1);
    assert_eq!(
        doc.session().tree().span(doc.session().tree().root()).end,
        5
    );
}

#[test]
fn untouched_region_is_reused_through_document_layer() {
    let (engine, split_calls, settle_calls) = make_engine();
    let mut doc = open("abcdefghij");

    doc.apply_changes(&engine, 1, &[], &SerialExecutor, &CancelToken::new());
    let root = doc.session().tree().root();
    let left = doc.session().tree().children(root)[0];

    // Same-length replace of byte 7 (line 0, character 7): right half only.
    doc.apply_changes(
        &engine,
        2,
        &[change(Some((0, 7, 0, 8)), "9")],
        &SerialExecutor,
        &CancelToken::new(),
    );

    let kids = doc.session().tree().children(root);
    assert_eq!(kids[0], left, "left region must keep its identity");
    assert_eq!(doc.session().tree().status(left), Status::Done);
    assert_eq!(split_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        settle_calls.load(Ordering::SeqCst),
        3,
        "only the right half re-settles"
    );
    assert_eq!(doc.text(), "abcdefg9ij");
}

#[test]
fn diagnostics_convert_spans_with_multibyte_text() {
    let mut schedule = Schedule::new();
    schedule.push(FailOnBang);
    let engine = Engine::new(schedule);

    // 13 bytes; 10 UTF-16 units ("😀" costs two surrogate units).
    let mut doc = open("héllo 😀 !");
    doc.apply_changes(&engine, 1, &[], &SerialExecutor, &CancelToken::new());

    let diags = diagnostics(&doc, DiagnosticsOptions::default(), |node| {
        assert_eq!(node.status, Status::Failed);
        Some(Diagnostic {
            message: "bang".into(),
            ..Diagnostic::default()
        })
    });

    assert_eq!(diags.len(), 1);
    assert_eq!(
        diags[0].range.start,
        Position {
            line: 0,
            character: 0
        }
    );
    assert_eq!(
        diags[0].range.end,
        Position {
            line: 0,
            character: 10
        }
    );
}

#[test]
fn diagnostics_can_be_filtered_by_context() {
    let (engine, _split, _settle) = make_engine();
    let mut doc = open("abcdefghij");
    doc.apply_changes(&engine, 1, &[], &SerialExecutor, &CancelToken::new());

    // Nothing failed: an unconditional hook still sees no candidates.
    let diags = diagnostics(&doc, DiagnosticsOptions::default(), |_| {
        Some(Diagnostic::default())
    });
    assert!(diags.is_empty());
}

#[test]
fn cascading_unparsed_regions_are_collapsed_by_default() {
    let (engine, _split, _settle) = make_engine();
    let mut doc = open("abcdefghij");
    doc.apply_changes(&engine, 1, &[], &SerialExecutor, &CancelToken::new());

    // Invalidate root and left region without re-running: both end up
    // Unparsed, and left sits under the (unparsed) root.
    doc.session_mut().edit(increparse::Edit::insert(3, 1));

    let topmost = diagnostics(&doc, DiagnosticsOptions::default(), |_| {
        Some(Diagnostic::default())
    });
    assert_eq!(topmost.len(), 1, "left is under the failing root: skipped");

    let all = diagnostics(&doc, DiagnosticsOptions { cascades: true }, |_| {
        Some(Diagnostic::default())
    });
    assert_eq!(all.len(), 2);
}
