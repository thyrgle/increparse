//! The builder-shaped `Language` implementation for the common case.

use std::sync::Arc;

use increparse::Engine;
use lsp_types::{CompletionResponse, Diagnostic, DocumentSymbol, Hover, Location, SymbolKind};

use crate::document::Document;
use crate::encoding::PositionEncoding;
use crate::server::Language;
use crate::FailedNode;

/// The type of the [`SimpleLanguage::diagnostic_fn`] hook.
pub type DiagnosticFn<C> =
    dyn Fn(&Document<C>, &FailedNode<'_, C>) -> Option<Diagnostic> + Send + Sync;

/// The type of the [`SimpleLanguage::extra_diagnostics`] hook.
pub type ExtraDiagnosticsFn<C> = dyn Fn(&Document<C>) -> Vec<Diagnostic> + Send + Sync;

/// The type of the [`SimpleLanguage::symbols_fn`] hook.
pub type SymbolsFn<C> = dyn Fn(&Document<C>) -> Vec<DocumentSymbol> + Send + Sync;

/// The type of the [`SimpleLanguage::label_fn`] hook.
pub type LabelFn<C> = dyn Fn(&C) -> Option<NodeLabel> + Send + Sync;

/// A [`Language`] built from closures — the Rust equivalent of a Lua
/// language definition.
///
/// For the common server you never implement the [`Language`] trait;
/// describe the language and hand the builder to
/// [`serve`](crate::serve):
///
/// ```
/// use increparse::Engine;
/// use increparse_lsp::{Document, FailedNode, SimpleLanguage};
/// use lsp_types::Diagnostic;
///
/// # #[derive(Clone, Debug, PartialEq, Eq)]
/// # enum Ctx { File }
/// # fn build(engine: Engine<Ctx>) {
/// let language = SimpleLanguage::new(engine, Ctx::File)
///     .diagnostic_fn(|_doc, node| {
///         Some(Diagnostic { message: "could not parse".into(), ..Diagnostic::default() })
///     })
///     .label_fn(|ctx| Some(increparse_lsp::NodeLabel::new("region")));
/// # let _ = language;
/// # }
/// ```
pub struct SimpleLanguage<C> {
    engine: Engine<C>,
    root_ctx: C,
    encoding: PositionEncoding,
    diagnostic_fn: Option<Arc<DiagnosticFn<C>>>,
    extra_diagnostics_fn: Option<Arc<ExtraDiagnosticsFn<C>>>,
    symbols_fn: Option<Arc<SymbolsFn<C>>>,
    label_fn: Option<Arc<LabelFn<C>>>,
    describe_fn: Option<Arc<DescribeFn<C>>>,
    hover_fn: Option<Arc<HoverFn<C>>>,
    definition_fn: Option<Arc<DefinitionFn<C>>>,
    completion_fn: Option<Arc<CompletionFn<C>>>,
    code_action_fn: Option<Arc<CodeActionFn<C>>>,
}

/// The type of the [`SimpleLanguage::describe_fn`] hook: describe a
/// context in one sentence and the skeleton turns it into hover contents.
pub type DescribeFn<C> = dyn Fn(&C) -> Option<String> + Send + Sync;

/// The type of the [`SimpleLanguage::hover_fn`] hook (full control).
pub type HoverFn<C> = dyn Fn(&Document<C>, usize) -> Option<Hover> + Send + Sync;

/// The type of the [`SimpleLanguage::definition_fn`] hook.
pub type DefinitionFn<C> = dyn Fn(&Document<C>, usize) -> Option<Vec<Location>> + Send + Sync;

/// The type of the [`SimpleLanguage::completion_fn`] hook.
pub type CompletionFn<C> = dyn Fn(&Document<C>, usize) -> Option<CompletionResponse> + Send + Sync;

/// The type of the [`SimpleLanguage::code_action_fn`] hook: code
/// actions (typically quickfixes) for a range.
pub type CodeActionFn<C> =
    dyn Fn(&Document<C>, lsp_types::Range) -> Vec<lsp_types::CodeAction> + Send + Sync;

/// A display name (and optional detail) for one tree node — what
/// [`SimpleLanguage::label_fn`] returns to power the outline view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeLabel {
    /// The symbol's name.
    pub name: String,
    /// Optional detail shown next to the name (e.g. a parameter list).
    pub detail: Option<String>,
    /// The outline kind (defaults to [`SymbolKind::FUNCTION`] — override it
    /// for languages whose named things are rules, sections, selectors,
    /// recipes, ...).
    pub kind: SymbolKind,
}

impl NodeLabel {
    /// Creates a label with no detail and the default kind.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            detail: None,
            kind: SymbolKind::FUNCTION,
        }
    }

    /// Sets the detail string.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Sets the outline kind — a language's named things need not be
    /// functions.
    pub fn kind(mut self, kind: SymbolKind) -> Self {
        self.kind = kind;
        self
    }
}

impl<C: Clone + PartialEq + Send + 'static> SimpleLanguage<C> {
    /// Creates a builder over an engine and a root context.
    pub fn new(engine: Engine<C>, root_ctx: C) -> Self {
        Self {
            engine,
            root_ctx,
            encoding: PositionEncoding::Utf16,
            diagnostic_fn: None,
            extra_diagnostics_fn: None,
            symbols_fn: None,
            label_fn: None,
            describe_fn: None,
            hover_fn: None,
            definition_fn: None,
            completion_fn: None,
            code_action_fn: None,
        }
    }

    /// Sets the negotiated position encoding (default UTF-16).
    pub fn encoding(mut self, encoding: PositionEncoding) -> Self {
        self.encoding = encoding;
        self
    }

    /// How a failing region becomes a diagnostic; `None` stays silent.
    /// Diagnostics returned with an empty range are filled in from the
    /// node's span.
    pub fn diagnostic_fn(
        mut self,
        f: impl Fn(&Document<C>, &FailedNode<'_, C>) -> Option<Diagnostic> + Send + Sync + 'static,
    ) -> Self {
        self.diagnostic_fn = Some(Arc::new(f));
        self
    }

    /// Diagnostics that do not come from parse failures — lint-rule
    /// violations, style warnings, anything computed from the settled
    /// document. Called once per publish, after the parse pass; its output
    /// is appended to the parse diagnostics.
    pub fn extra_diagnostics(
        mut self,
        f: impl Fn(&Document<C>) -> Vec<Diagnostic> + Send + Sync + 'static,
    ) -> Self {
        self.extra_diagnostics_fn = Some(Arc::new(f));
        self
    }

    /// Full-control outline symbols. Mutually exclusive in spirit with
    /// [`label_fn`](Self::label_fn) — when both are set, this wins.
    pub fn symbols_fn(
        mut self,
        f: impl Fn(&Document<C>) -> Vec<DocumentSymbol> + Send + Sync + 'static,
    ) -> Self {
        self.symbols_fn = Some(Arc::new(f));
        self
    }

    /// Names tree nodes; every named node becomes an outline symbol with
    /// the node's range — symbols without writing a tree walk.
    pub fn label_fn(mut self, f: impl Fn(&C) -> Option<NodeLabel> + Send + Sync + 'static) -> Self {
        self.label_fn = Some(Arc::new(f));
        self
    }

    /// One-sentence hover: describe a context and the skeleton finds the
    /// node under the cursor, builds the hover contents, and attaches the
    /// node's range. The friendliest way to add hover — nothing about the
    /// language's shape is assumed.
    pub fn describe_fn(mut self, f: impl Fn(&C) -> Option<String> + Send + Sync + 'static) -> Self {
        self.describe_fn = Some(Arc::new(f));
        self
    }

    /// Full-control hover: receives the document and the cursor's byte
    /// offset (position encoding already handled).
    pub fn hover_fn(
        mut self,
        f: impl Fn(&Document<C>, usize) -> Option<Hover> + Send + Sync + 'static,
    ) -> Self {
        self.hover_fn = Some(Arc::new(f));
        self
    }

    /// Go-to-definition: return the locations to jump to.
    pub fn definition_fn(
        mut self,
        f: impl Fn(&Document<C>, usize) -> Option<Vec<Location>> + Send + Sync + 'static,
    ) -> Self {
        self.definition_fn = Some(Arc::new(f));
        self
    }

    /// Completions for the cursor position.
    pub fn completion_fn(
        mut self,
        f: impl Fn(&Document<C>, usize) -> Option<CompletionResponse> + Send + Sync + 'static,
    ) -> Self {
        self.completion_fn = Some(Arc::new(f));
        self
    }

    /// Code actions for a range — typically quickfixes for the
    /// diagnostics published there.
    pub fn code_action_fn(
        mut self,
        f: impl Fn(&Document<C>, lsp_types::Range) -> Vec<lsp_types::CodeAction> + Send + Sync + 'static,
    ) -> Self {
        self.code_action_fn = Some(Arc::new(f));
        self
    }
}

/// `C` must additionally be `Sync` because the stored closures accept
/// `&C` from any thread.
impl<C: Clone + PartialEq + Send + Sync + 'static> Language<C> for SimpleLanguage<C> {
    fn supports_symbols(&self) -> bool {
        self.symbols_fn.is_some() || self.label_fn.is_some()
    }

    fn supports_hover(&self) -> bool {
        self.hover_fn.is_some() || self.describe_fn.is_some()
    }

    fn supports_definition(&self) -> bool {
        self.definition_fn.is_some()
    }

    fn supports_completion(&self) -> bool {
        self.completion_fn.is_some()
    }

    fn supports_code_actions(&self) -> bool {
        self.code_action_fn.is_some()
    }

    fn code_action(
        &self,
        doc: &Document<C>,
        range: lsp_types::Range,
    ) -> Vec<lsp_types::CodeAction> {
        match &self.code_action_fn {
            Some(f) => f(doc, range),
            None => Vec::new(),
        }
    }

    fn hover(&self, doc: &Document<C>, offset: usize) -> Option<Hover> {
        if let Some(f) = &self.hover_fn {
            return f(doc, offset);
        }
        let describe = self.describe_fn.as_ref()?;
        let tree = doc.session().tree();
        let id = tree.node_at(offset)?;
        let text = describe(tree.ctx(id))?;
        Some(Hover {
            contents: lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(text)),
            range: Some(doc.range(tree.span(id))),
        })
    }

    fn definition(&self, doc: &Document<C>, offset: usize) -> Option<Vec<Location>> {
        let f = self.definition_fn.as_ref()?;
        f(doc, offset)
    }

    fn completion(&self, doc: &Document<C>, offset: usize) -> Option<CompletionResponse> {
        let f = self.completion_fn.as_ref()?;
        f(doc, offset)
    }

    fn engine(&self) -> &Engine<C> {
        &self.engine
    }

    fn root_ctx(&self) -> C {
        self.root_ctx.clone()
    }

    fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    fn diagnostic(&self, doc: &Document<C>, node: FailedNode<'_, C>) -> Option<Diagnostic> {
        let f = self.diagnostic_fn.as_ref()?;
        f(doc, &node)
    }

    fn extra_diagnostics(&self, doc: &Document<C>) -> Vec<Diagnostic> {
        match &self.extra_diagnostics_fn {
            Some(f) => f(doc),
            None => Vec::new(),
        }
    }

    fn symbols(&self, doc: &Document<C>) -> Vec<DocumentSymbol> {
        if let Some(f) = &self.symbols_fn {
            return f(doc);
        }
        if let Some(label) = &self.label_fn {
            let tree = doc.session().tree();
            return tree
                .nodes()
                .filter_map(|id| {
                    let label = label(tree.ctx(id))?;
                    let range = doc.range(tree.span(id));
                    Some(DocumentSymbol {
                        name: label.name,
                        detail: label.detail,
                        kind: label.kind,
                        range,
                        selection_range: range,
                        children: None,
                        tags: None,
                        #[allow(deprecated)]
                        deprecated: None,
                    })
                })
                .collect();
        }
        Vec::new()
    }
}
