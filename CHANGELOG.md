# Changelog

All notable changes to the increparse workspace are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/); versions follow
semver.

## [Unreleased]

### Added

- Criterion benchmarks (`benches/parse.rs`): cold parse of a 2,000-line
  corpus, in-place re-parse, and mid-file edit re-parse.
- Property-based invariant tests (`proptest`): runs terminate without
  panicking on arbitrary UTF-8 input, trees stay well-formed (spans in
  bounds, contained in parents, status counts consistent), runs are
  deterministic, and contract-violating children are rejected and
  reported.

### Changed

- CI: format check, clippy `-D warnings`, doc build, per-crate package
  dry-runs, and a test matrix over default/`--all-features`.
- Workspace doc comments: resolved broken intra-doc links and removed
  redundant explicit link targets.

### Fixed

- delint bin 1.2.1: rustdoc warnings (broken link, unclosed HTML tags)
  that failed the docs.rs build.

## Published

### increparse-lsp 0.3.0

- `textDocument/codeAction` support: `Language::code_action` +
  `SimpleLanguage::code_action_fn` hooks.

### increparse-lua 0.1.1

- Requires increparse-lsp 0.3.0 (the 0.2.0 requirement could not
  resolve against the published 0.3.0).

### increparse 0.1.0 / increparse-lsp 0.2.0 / increparse-nom 0.1.0 / increparse-chumsky 0.1.0

- Initial releases: multi-pass fixpoint engine, LSP adapter (diagnostics,
  hover, symbols, completion hooks), nom/chumsky pass adapters.
