//! Wrap [chumsky](https://docs.rs/chumsky) parsers in increparse
//! [`Pass`]es.
//!
//! chumsky 0.10 reports spans as [`SimpleSpan`]s relative to the input slice
//! it parsed; increparse needs *absolute* spans carrying a source revision —
//! and the incremental tree only reuses regions whose spans match exactly.
//! This crate does the translation once, correctly, so your chumsky parsers
//! can stay in slice-relative coordinates.
//!
//! A pass is a function from the region's slice to a chumsky
//! [`ParseResult`] whose output is the parsed children: each child is a
//! **slice-relative** [`SimpleSpan`] plus a context value for the next
//! round. The wrapper converts the result to an
//! [`Outcome`] (from the core crate):
//!
//! * parse succeeded with children → `Outcome::Expand` with rebased
//!   absolute spans,
//! * succeeded with no children → `Outcome::Done`,
//! * parse failed → `Outcome::Failed` (errors dropped; the region stays in
//!   the tree as a leaf and later passes may retry it).
//!
//! Note: chumsky's `Parser::parse` implicitly requires the *whole* input to
//! be consumed. To claim children and ignore the rest of the region, end
//! your parser with `.then_ignore(any().repeated())`.
//!
//! Because parsers are typically built per-call (they are cheap), the
//! closure receives the slice and runs its parser itself:
//!
//! # Examples
//!
//! ```
//! use chumsky::prelude::*;
//! use increparse::{Outcome, Pass, Span};
//! use increparse_chumsky::{chumsky_pass, ChumChildren};
//!
//! // Parsers are built per call, tied to the input's lifetime.
//! fn split_at_hi(slice: &str) -> ParseResult<ChumChildren<()>, Rich<'_, char>> {
//!     fn parser<'a>() -> impl Parser<'a, &'a str, ChumChildren<()>, extra::Err<Rich<'a, char>>> {
//!         just("hi")
//!             .map_with(|_out, e| vec![(e.span(), ())])
//!             .then_ignore(any().repeated())
//!     }
//!     parser().parse(slice)
//! }
//!
//! let pass = chumsky_pass(split_at_hi);
//! let source = "hi there";
//! match pass.parse(source, Span::new(0, source.len(), 0), &()) {
//!     Outcome::Expand(children) => {
//!         // The child span is absolute, even though chumsky saw a slice.
//!         assert_eq!(children[0].0.start, 0);
//!         assert_eq!(children[0].0.end, 2);
//!     }
//!     _ => panic!("expected expansion"),
//! }
//! ```
//!
//! Spans you emit are validated by the engine like any other: they must be
//! contained in the region and (unless
//! [`EngineConfig::enforce_shrink`](increparse::EngineConfig::enforce_shrink)
//! is disabled) strictly smaller.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use chumsky::prelude::Rich;
use chumsky::span::SimpleSpan;
use chumsky::ParseResult;
use increparse::{Outcome, Pass, Span};

/// The children a chumsky parser produces: slice-relative spans plus the
/// context each child carries into the next round.
pub type ChumChildren<C> = Vec<(SimpleSpan, C)>;

/// A [`Pass`] driven by a chumsky parser.
///
/// See the [crate docs](self) for the conversion rules. Build one with
/// [`chumsky_pass`].
#[derive(Debug, Clone, Copy)]
pub struct ChumskyPass<F> {
    f: F,
}

/// Creates a pass from a closure that parses a slice into [`ChumChildren`].
///
/// chumsky 0.10's chumsky::Parser trait is tied to the input lifetime, so parsers
/// are built per call (they are cheap) by a small factory function; the
/// closure builds one and runs it against the slice:
///
/// ```
/// use chumsky::prelude::*;
/// use increparse::{Outcome, Pass, Span};
/// use increparse_chumsky::{chumsky_pass, ChumChildren};
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum Ctx {
///     Ident,
/// }
///
/// // The factory ties the parser to the input lifetime...
/// fn idents<'a>() -> impl Parser<'a, &'a str, ChumChildren<Ctx>, extra::Err<Rich<'a, char>>> {
///     text::ident()
///         .map_with(|_name, e| vec![(e.span(), Ctx::Ident)])
///         .then_ignore(any().repeated())
/// }
///
/// // ...and the closure is the pass body.
/// fn idents_pass(slice: &str) -> ParseResult<ChumChildren<Ctx>, Rich<'_, char>> {
///     idents().parse(slice)
/// }
///
/// let pass = chumsky_pass(idents_pass);
/// let source = "one two";
/// assert!(matches!(pass.parse(source, Span::new(0, source.len(), 0), &Ctx::Ident),
///     Outcome::Expand(_)));
/// ```
pub fn chumsky_pass<F, C>(f: F) -> ChumskyPass<F>
where
    F: for<'a> Fn(&'a str) -> ParseResult<ChumChildren<C>, Rich<'a, char>>,
{
    ChumskyPass { f }
}

impl<C, F> Pass for ChumskyPass<F>
where
    F: for<'a> Fn(&'a str) -> ParseResult<ChumChildren<C>, Rich<'a, char>> + Send + Sync,
{
    type Ctx = C;

    fn parse(&self, source: &str, span: Span, _ctx: &C) -> Outcome<C> {
        let slice = &source[span.to_range()];
        let result = (self.f)(slice).into_result();
        match result {
            Ok(children) => {
                if children.is_empty() {
                    return Outcome::Done;
                }
                let children = children
                    .into_iter()
                    .map(|(simple, ctx)| {
                        (
                            Span::new(span.start + simple.start, span.start + simple.end, span.rev),
                            ctx,
                        )
                    })
                    .collect();
                Outcome::Expand(children)
            }
            Err(_) => Outcome::Failed,
        }
    }

    fn name(&self) -> &'static str {
        "ChumskyPass"
    }
}
