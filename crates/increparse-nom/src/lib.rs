//! Wrap [`nom`] parsers in increparse [`Pass`]es.
//!
//! nom reports offsets relative to the input slice it was handed; increparse
//! needs *absolute* spans carrying a source revision — and the incremental
//! tree only reuses regions whose spans match exactly. Getting that
//! translation wrong by one byte silently degrades re-parsing into
//! re-parsing-everything. This crate does it once, correctly, so your nom
//! parsers can stay in slice-relative coordinates.
//!
//! A pass is a function from a [`LocatedSpan`] of the region's text to an
//! `IResult` whose output is the parsed children: each child is a
//! **slice-relative** `Range<usize>` plus a context value for the next
//! round. The wrapper converts the result to an
//! [`Outcome`] (from the core crate):
//!
//! * `Ok(children)` → `Outcome::Expand` with rebased absolute spans,
//! * `Ok(vec![])` → `Outcome::Done` (nothing left to parse in the region),
//! * `Err(_)` → `Outcome::Failed` (the error is dropped; the region stays
//!   in the tree as a leaf and later passes may retry it).
//!
//! # Examples
//!
//! ```
//! use increparse::{Outcome, Pass, Span};
//! use increparse_nom::{nom_pass, NomChildren};
//! use nom::IResult;
//! use nom::bytes::complete::tag;
//! use nom_locate::LocatedSpan;
//! use std::ops::Range;
//!
//! fn split_at_hi(i: LocatedSpan<&str>) -> IResult<LocatedSpan<&str>, NomChildren<()>> {
//!     let (i, _) = tag("hi")(i)?;
//!     let end = i.location_offset();
//!     Ok((i, vec![(0..end, ())]))
//! }
//!
//! let pass = nom_pass(split_at_hi);
//! let source = "hi there";
//! match pass.parse(source, Span::new(0, source.len(), 0), &()) {
//!     Outcome::Expand(children) => {
//!         // The child span is absolute, even though nom saw only a slice.
//!         assert_eq!(children[0].0, Span::new(0, 2, 0));
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

use increparse::{Outcome, Pass, Span};
use nom::IResult;
use nom_locate::LocatedSpan;
use std::ops::Range;

/// The children a nom parser produces: slice-relative ranges plus the
/// context each child carries into the next round.
pub type NomChildren<C> = Vec<(Range<usize>, C)>;

/// The input and remaining-output type nom parsers see: a located view of
/// the region's slice of the source.
pub type Located<'a> = LocatedSpan<&'a str>;

/// A [`Pass`] driven by a nom parser.
///
/// See the [crate docs](self) for the conversion rules. Build one with
/// [`nom_pass`].
#[derive(Debug, Clone, Copy)]
pub struct NomPass<F> {
    f: F,
}

/// Creates a pass from a nom parser producing [`NomChildren`].
pub fn nom_pass<C, F>(f: F) -> NomPass<F>
where
    F: for<'a> Fn(Located<'a>) -> IResult<Located<'a>, NomChildren<C>>,
{
    NomPass { f }
}

impl<C, F> Pass for NomPass<F>
where
    F: for<'a> Fn(Located<'a>) -> IResult<Located<'a>, NomChildren<C>> + Send + Sync,
{
    type Ctx = C;

    fn parse(&self, source: &str, span: Span, _ctx: &C) -> Outcome<C> {
        let slice = &source[span.to_range()];
        let input = LocatedSpan::new(slice);
        match (self.f)(input) {
            Ok((_rest, children)) => {
                if children.is_empty() {
                    return Outcome::Done;
                }
                let children = children
                    .into_iter()
                    .map(|(range, ctx)| {
                        (
                            Span::new(span.start + range.start, span.start + range.end, span.rev),
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
        "NomPass"
    }
}
