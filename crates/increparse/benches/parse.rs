//! Benchmarks for the parse engine: cold parse, incremental re-parse,
//! and a sustained edit loop on a synthetic 2,000-line corpus.
//!
//! The corpus is deterministic (LCG-generated), so numbers are
//! comparable across runs on the same machine.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use increparse::prelude::*;
use increparse::Edit;

#[derive(Clone, Debug, PartialEq, Eq)]
enum LangCtx {
    File,
    Function { name: String, params: Vec<String> },
    Return { function: String },
}

// ---- deterministic corpus ----

struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }
}

fn corpus(lines: usize) -> String {
    let mut rng = Lcg(0x1234_5678);
    let mut out = String::new();
    for i in 0..lines {
        match rng.next() % 8 {
            0..=5 => out.push_str(&format!(
                "def fn_{i}(a{0}, b{0}) {{ return a{0} + b{0}; }}\n",
                i
            )),
            6 => out.push_str(&format!("def bad_{i}() {{ return; }}\n")),
            7 => out.push_str(&format!("// noise line {i}\n")),
            _ => unreachable!(),
        }
    }
    out
}

// ---- toy passes (mirrors examples/mini_lang.rs) ----

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn read_ident(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    if i >= bytes.len() || !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        return None;
    }
    let mut end = i + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    Some((i, end))
}

fn match_brace(bytes: &[u8], open: usize, limit: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate().take(limit).skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_params(bytes: &[u8], start: usize, end: usize) -> Option<Vec<String>> {
    let text = std::str::from_utf8(&bytes[start..end]).ok()?;
    let mut params = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let ident = read_ident(part.as_bytes(), 0)?;
        if ident.1 != part.len() {
            return None;
        }
        params.push(part.to_string());
    }
    Some(params)
}

fn functions_pass(source: &str, span: Span, ctx: &LangCtx) -> Outcome<LangCtx> {
    if !matches!(ctx, LangCtx::File) {
        return Outcome::Failed;
    }
    let bytes = source.as_bytes();
    let mut children = Vec::new();
    let mut i = span.start;
    while i < span.end {
        i = skip_ws(bytes, i);
        let Some((kw_start, kw_end)) = read_ident(bytes, i) else {
            i += 1;
            continue;
        };
        if &source[kw_start..kw_end] != "def" {
            i = kw_end;
            continue;
        }
        let name_pos = skip_ws(bytes, kw_end);
        let Some((name_start, name_end)) = read_ident(bytes, name_pos) else {
            i = kw_end;
            continue;
        };
        let open_paren = skip_ws(bytes, name_end);
        if open_paren >= span.end || bytes[open_paren] != b'(' {
            i = kw_end;
            continue;
        }
        let line_end = (open_paren..span.end)
            .find(|&j| bytes[j] == b'\n')
            .unwrap_or(span.end);
        let Some(close_paren) = (open_paren + 1..line_end).find(|&j| bytes[j] == b')') else {
            i = line_end + 1;
            continue;
        };
        let Some(params) = parse_params(bytes, open_paren + 1, close_paren) else {
            i = close_paren + 1;
            continue;
        };
        let open_brace = skip_ws(bytes, close_paren + 1);
        if open_brace >= span.end || bytes[open_brace] != b'{' {
            i = close_paren + 1;
            continue;
        }
        let Some(close_brace) = match_brace(bytes, open_brace, span.end) else {
            break;
        };
        children.push((
            Span::new(i, close_brace + 1, span.rev),
            LangCtx::Function {
                name: source[name_start..name_end].to_string(),
                params,
            },
        ));
        i = close_brace + 1;
    }
    Outcome::Expand(children)
}

fn body_pass(source: &str, span: Span, ctx: &LangCtx) -> Outcome<LangCtx> {
    let LangCtx::Function { .. } = ctx else {
        return Outcome::Failed;
    };
    let bytes = source.as_bytes();
    let mut children = Vec::new();
    let mut i = span.start;
    while i < span.end {
        i = skip_ws(bytes, i);
        if source[i..].starts_with("return") {
            let Some(semi) = (i..span.end).find(|&j| bytes[j] == b';') else {
                break;
            };
            children.push((
                Span::new(i, semi + 1, span.rev),
                LangCtx::Return {
                    function: String::new(),
                },
            ));
            i = semi + 1;
        } else {
            i += 1;
        }
    }
    Outcome::Expand(children)
}

fn make_engine() -> Engine<LangCtx> {
    Engine::with((pass_fn(functions_pass), pass_fn(body_pass)))
}

fn criterion_benches(c: &mut Criterion) {
    let source = corpus(2_000);
    let engine = make_engine();

    c.bench_function("cold_parse/2000_lines", |b| {
        b.iter(|| {
            let mut session = Session::from_source(black_box(&source), 0, LangCtx::File);
            session.run(
                black_box(&engine),
                &source,
                &SerialExecutor,
                &CancelToken::new(),
            )
        })
    });

    // Warm benchmark: one incremental edit per iteration. The edit is a
    // same-length replacement, so the corpus size stays constant and
    // every iteration does the same amount of remap + re-parse work.
    let mut session = Session::from_source(&source, 0, LangCtx::File);
    session.run(&engine, &source, &SerialExecutor, &CancelToken::new());

    // locate the first `return ...;` region (created by body_pass)
    let mut target = None;
    for id in session.tree().nodes() {
        if matches!(session.tree().ctx(id), LangCtx::Return { .. }) {
            target = Some(session.tree().span(id));
            break;
        }
    }
    let Some(span) = target else {
        eprintln!("bench source changed: return region missing");
        return;
    };
    let replacement = {
        let mut r = b"return 9;".to_vec();
        r.resize(span.len(), b' ');
        r
    };

    c.bench_function("reparse/in_place_edit_2000_lines", |b| {
        b.iter(|| {
            let edit = Edit::replace(span.start, span.end, span.start + replacement.len());
            session.edit(edit);
            session.run(&engine, &source, &SerialExecutor, &CancelToken::new())
        })
    });

    // A mid-file same-length edit forces remapping of everything after
    // it — the pessimistic case.
    c.bench_function("reparse/mid_file_edit_2000_lines", |b| {
        b.iter(|| {
            let mid = source.len() / 2;
            let edit = Edit::replace(mid, mid + 2, mid + 2);
            session.edit(edit);
            session.run(&engine, &source, &SerialExecutor, &CancelToken::new())
        })
    });
}

criterion_group!(benches, criterion_benches);
criterion_main!(benches);
