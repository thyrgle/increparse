//! Define an increparse language server in **pure Lua**.
//!
//! A language is a single Lua file returning a table:
//!
//! ```lua
//! -- minilang.lua
//! return {
//!   name = "minilang",
//!   root_ctx = { File = true },
//!   -- round r of a run calls passes[r]
//!   passes = {
//!     function(source, span, ctx)
//!       -- span = { start = , end = , rev = }
//!       -- return { expand = { { start = , end = , ctx = ... }, ... } }
//!       --     or "done" / "failed" / nil
//!       return "done"
//!     end,
//!   },
//!   -- optional: how a failing region becomes a diagnostic
//!   diagnostic = function(source, node) return { message = "oops" } end,
//!   -- optional: outline symbols from a snapshot of the tree
//!   symbols = function(nodes) return {} end,
//! }
//! ```
//!
//! Hand the file to the bundled binary and point any LSP client at it:
//!
//! ```text
//! increparse-lua-server ~/.config/minilang/lang.lua
//! ```
//!
//! # Semantics
//!
//! * Passes receive `(source, span, ctx)` where child ranges in the outcome
//!   are **absolute** byte ranges into `source` — no rebasing traps.
//! * Outcomes: a table with an `expand` array expands the region; `"done"`
//!   accepts it; `"failed"` or `nil` marks it failed (later passes retry).
//!   A Lua error thrown inside a pass is caught and treated as `Failed` —
//!   a broken pass never takes the server down.
//! * Contexts are arbitrary Lua values. Region reuse after an edit matches
//!   contexts by **deep equality** (tables compared recursively), so edits
//!   re-parse only the touched chain — the same incremental behaviour the
//!   Rust side gets.
//! * `diagnostic` receives `(source, node)` with
//!   `node = { ctx, status, start, end }` (`status` is `"failed"` or
//!   `"unparsed"`); returning a table with a `message` publishes a
//!   diagnostic at the node's range (optionally set `start`/`end` there).
//! * `symbols` receives an array of `{ start, end, status, ctx }` snapshots
//!   and returns `{ { name = , detail = , start = , end = } }`; ranges are
//!   byte ranges, converted to editor positions by the skeleton.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::error::Error;
use std::path::Path;

use increparse::{Engine, Outcome, Pass, Schedule, Span};
use increparse_lsp::{Document, FailedNode, Language};
use lsp_types::{Diagnostic, DiagnosticSeverity, DocumentSymbol, SymbolKind};
use mlua::{Function, Lua, Table, Value};

/// A context value from Lua: any [`Value`], compared by deep equality.
///
/// Deep equality is what lets the incremental tree reuse regions across
/// edits: two contexts match only if they are structurally identical
/// (tables recursively; `1` and `1.0` compare equal; functions and other
/// opaque values never compare equal — contexts should be data).
#[derive(Debug, Clone)]
pub struct LuaCtx(pub Value);

impl PartialEq for LuaCtx {
    fn eq(&self, other: &Self) -> bool {
        deep_eq(&self.0, &other.0)
    }
}

impl Eq for LuaCtx {}

fn deep_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Boolean(x), Value::Boolean(y)) => x == y,
        (Value::Integer(x), Value::Integer(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::Integer(x), Value::Number(y)) | (Value::Number(y), Value::Integer(x)) => {
            (*x as f64) == *y
        }
        (Value::String(x), Value::String(y)) => x.as_bytes() == y.as_bytes(),
        (Value::Table(x), Value::Table(y)) => {
            let a: Vec<(Value, Value)> = x.pairs().filter_map(|p| p.ok()).collect();
            let b: Vec<(Value, Value)> = y.pairs().filter_map(|p| p.ok()).collect();
            if a.len() != b.len() {
                return false;
            }
            a.iter()
                .all(|(k, v)| b.iter().any(|(k2, v2)| deep_eq(k, k2) && deep_eq(v, v2)))
        }
        _ => false,
    }
}

fn status_name(status: increparse::Status) -> &'static str {
    match status {
        increparse::Status::Unparsed => "unparsed",
        increparse::Status::Expanded => "expanded",
        increparse::Status::Done => "done",
        increparse::Status::Failed => "failed",
    }
}

/// A [`Pass`] backed by a Lua function.
#[derive(Clone)]
pub(crate) struct LuaPass {
    lua: Lua,
    function: Function,
    index: usize,
}

impl LuaPass {
    fn try_parse(
        &self,
        source: &str,
        span: Span,
        ctx: &LuaCtx,
    ) -> Result<Outcome<LuaCtx>, Box<dyn Error + Send + Sync>> {
        let lua = &self.lua;
        let span_table = lua.create_table()?;
        span_table.set("start", span.start)?;
        span_table.set("end", span.end)?;
        span_table.set("rev", span.rev)?;

        let result: Value = self.function.call((source, span_table, ctx.0.clone()))?;

        match result {
            Value::Nil => Ok(Outcome::Failed),
            Value::String(s) if s.as_bytes() == b"done" => Ok(Outcome::Done),
            Value::String(s) if s.as_bytes() == b"failed" => Ok(Outcome::Failed),
            Value::Table(outcome) => {
                let expand: Value = outcome.get("expand")?;
                let Value::Table(array) = expand else {
                    eprintln!(
                        "increparse-lua: pass #{} returned a table without `expand`; treating as failed",
                        self.index
                    );
                    return Ok(Outcome::Failed);
                };
                let mut children = Vec::new();
                for item in array.sequence_values::<Table>() {
                    let item = item?;
                    let start: usize = item.get("start")?;
                    let end: usize = item.get("end")?;
                    let child_ctx = LuaCtx(item.get::<Value>("ctx")?);
                    if start > end {
                        return Err(format!(
                            "pass #{} produced a child with start {start} > end {end}",
                            self.index
                        )
                        .into());
                    }
                    children.push((Span::new(start, end, span.rev), child_ctx));
                }
                Ok(Outcome::Expand(children))
            }
            other => {
                eprintln!(
                    "increparse-lua: pass #{} returned {}; treating as failed",
                    self.index,
                    other.type_name()
                );
                Ok(Outcome::Failed)
            }
        }
    }
}

impl Pass for LuaPass {
    type Ctx = LuaCtx;

    fn parse(&self, source: &str, span: Span, ctx: &LuaCtx) -> Outcome<LuaCtx> {
        match self.try_parse(source, span, ctx) {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("increparse-lua: pass #{} error: {err}", self.index);
                Outcome::Failed
            }
        }
    }

    fn name(&self) -> &'static str {
        "LuaPass"
    }
}

/// A language defined by a Lua configuration file.
///
/// Build with [`LuaLanguage::from_path`] and hand it to
/// increparse_lsp::serve:
///
/// ```no_run
/// fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
///     let language = increparse_lua::LuaLanguage::from_path("minilang.lua")?;
///     increparse_lsp::serve(language)
/// }
/// ```
pub struct LuaLanguage {
    lua: Lua,
    engine: Engine<LuaCtx>,
    root_ctx: LuaCtx,
    name: String,
    diagnostic_fn: Option<Function>,
    symbols_fn: Option<Function>,
}

impl LuaLanguage {
    /// Loads a language definition from a Lua file.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let lua = Lua::new();
        let script = std::fs::read_to_string(path)?;
        let config: Value = lua.load(script).eval()?;
        Self::from_config(lua, config)
    }

    /// Builds a language from an already-evaluated configuration table.
    pub fn from_config(lua: Lua, config: Value) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let Value::Table(table) = &config else {
            return Err("config must be a table".into());
        };

        let name: String = table.get("name")?;

        let root_ctx = LuaCtx(table.get("root_ctx")?);

        let passes_table: Table = table.get("passes")?;
        let mut functions = Vec::new();
        for f in passes_table.sequence_values::<Function>() {
            functions.push(f?);
        }
        if functions.is_empty() {
            return Err("config.passes must list at least one function".into());
        }

        let mut schedule = Schedule::new();
        for (index, function) in functions.into_iter().enumerate() {
            schedule.push(LuaPass {
                lua: lua.clone(),
                function,
                index,
            });
        }
        let engine = Engine::new(schedule);

        let diagnostic_fn = match table.get::<Value>("diagnostic")? {
            Value::Nil => None,
            Value::Function(f) => Some(f),
            other => {
                return Err(format!(
                    "config.diagnostic must be a function, got {}",
                    other.type_name()
                )
                .into())
            }
        };
        let symbols_fn = match table.get::<Value>("symbols")? {
            Value::Nil => None,
            Value::Function(f) => Some(f),
            other => {
                return Err(format!(
                    "config.symbols must be a function, got {}",
                    other.type_name()
                )
                .into())
            }
        };

        Ok(Self {
            lua,
            engine,
            root_ctx,
            name,
            diagnostic_fn,
            symbols_fn,
        })
    }

    /// The language's `name` from the config.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The embedded Lua state (contexts live here).
    pub fn lua(&self) -> &Lua {
        &self.lua
    }
}

impl Language<LuaCtx> for LuaLanguage {
    fn supports_symbols(&self) -> bool {
        self.symbols_fn.is_some()
    }

    fn engine(&self) -> &Engine<LuaCtx> {
        &self.engine
    }

    fn root_ctx(&self) -> LuaCtx {
        self.root_ctx.clone()
    }

    fn diagnostic(
        &self,
        doc: &Document<LuaCtx>,
        node: FailedNode<'_, LuaCtx>,
    ) -> Option<Diagnostic> {
        let function = self.diagnostic_fn.as_ref()?;
        let span = doc.session().tree().span(node.id);
        let node_table = self.lua.create_table().ok()?;
        node_table.set("ctx", node.ctx.0.clone()).ok()?;
        node_table.set("status", status_name(node.status)).ok()?;
        node_table.set("start", span.start).ok()?;
        node_table.set("end", span.end).ok()?;

        let result: Value = function.call((doc.text().to_string(), node_table)).ok()?;
        let Value::Table(diag) = result else {
            return None;
        };
        let message: String = diag.get("message").ok()?;
        let severity = diag
            .get::<i64>("severity")
            .ok()
            .and_then(|s| match s {
                1 => Some(DiagnosticSeverity::ERROR),
                2 => Some(DiagnosticSeverity::WARNING),
                3 => Some(DiagnosticSeverity::INFORMATION),
                4 => Some(DiagnosticSeverity::HINT),
                _ => None,
            })
            .or(Some(DiagnosticSeverity::ERROR));

        Some(Diagnostic {
            message,
            severity,
            ..Diagnostic::default()
        })
    }

    fn symbols(&self, doc: &Document<LuaCtx>) -> Vec<DocumentSymbol> {
        let Some(function) = &self.symbols_fn else {
            return Vec::new();
        };
        let Ok(nodes) = self.lua.create_table() else {
            return Vec::new();
        };
        let tree = doc.session().tree();
        for id in tree.nodes() {
            let Ok(entry) = self.lua.create_table() else {
                return Vec::new();
            };
            let span = tree.span(id);
            if entry.set("start", span.start).is_err()
                || entry.set("end", span.end).is_err()
                || entry.set("status", status_name(tree.status(id))).is_err()
                || entry.set("ctx", tree.ctx(id).0.clone()).is_err()
            {
                return Vec::new();
            }
            if nodes.push(entry).is_err() {
                return Vec::new();
            }
        }

        let result: Value = match function.call(nodes) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("increparse-lua: symbols error: {err}");
                return Vec::new();
            }
        };
        let Value::Table(array) = result else {
            return Vec::new();
        };

        let mut symbols = Vec::new();
        for item in array.sequence_values::<Table>() {
            let Ok(item) = item else { break };
            let Ok(name) = item.get::<String>("name") else {
                continue;
            };
            let (Ok(start), Ok(end)) = (item.get::<usize>("start"), item.get::<usize>("end"))
            else {
                continue;
            };
            let detail: Option<String> = item.get("detail").unwrap_or(None);
            let span = Span::new(start, end, doc.revision());
            let range = doc.range(span);
            symbols.push(DocumentSymbol {
                name,
                detail,
                kind: SymbolKind::FUNCTION,
                range,
                selection_range: range,
                children: None,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
            });
        }
        symbols
    }
}
