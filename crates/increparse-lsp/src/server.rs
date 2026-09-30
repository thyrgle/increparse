//! The `serve()` skeleton: implement one trait, get a complete server loop.
//!
//! Everything protocol-shaped lives here — the `initialize` handshake,
//! capability advertisement, document bookkeeping, change translation,
//! diagnostics publishing, `documentSymbol` dispatch, and the
//! [`Connection`]-by-value discipline that makes shutdown unable to
//! deadlock. Users describe *what* to parse and *how it fails*; this module
//! owns *how the server behaves*.

use std::collections::HashMap;
use std::error::Error;

use increparse::{CancelToken, Engine};
use lsp_server::Connection;
use lsp_types::{
    CodeActionParams, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, HoverParams, Location, OneOf,
    TextDocumentSyncCapability, TextDocumentSyncKind,
};

use crate::diagnostics::{self, DiagnosticsOptions, FailedNode};
use crate::document::Document;
use crate::encoding::PositionEncoding;

/// The document store behind a running server: URI -> parsed document.
#[allow(clippy::mutable_key_type)]
pub type Documents<C> = HashMap<lsp_types::Uri, Document<C>>;

/// The language-specific half of a server.
///
/// Implement this and hand it to [`serve`] (stdio) or [`serve_on`] (any
/// transport). Everything else — lifecycle, change bookkeeping, diagnostics
/// publishing, symbol dispatch — is the skeleton's job.///
/// # Examples
///
/// A minimal server that parses files and reports nothing:
///
/// ```
/// use increparse::{Engine, Outcome, Pass, Schedule, Span};
/// use increparse_lsp::{Document, FailedNode, Language, serve};
/// use lsp_types::Diagnostic;
///
/// #[derive(Clone, Debug, PartialEq, Eq)]
/// enum Ctx {
///     File,
/// }
///
/// struct Accept;
/// impl Pass for Accept {
///     type Ctx = Ctx;
///     fn parse(&self, _source: &str, _span: Span, _ctx: &Ctx) -> Outcome<Ctx> {
///         Outcome::Done
///     }
/// }
///
/// struct MyLang {
///     engine: Engine<Ctx>,
/// }
///
/// impl MyLang {
///     fn new() -> Self {
///         let mut schedule = Schedule::new();
///         schedule.push(Accept);
///         Self {
///             engine: Engine::new(schedule),
///         }
///     }
/// }
///
/// impl Language<Ctx> for MyLang {
///     fn engine(&self) -> &Engine<Ctx> {
///         &self.engine
///     }
///
///     fn root_ctx(&self) -> Ctx {
///         Ctx::File
///     }
///
///     fn diagnostic(&self, _doc: &Document<Ctx>, _node: FailedNode<'_, Ctx>) -> Option<Diagnostic> {
///         None
///     }
/// }
///
/// # fn untouched() {
/// // serve(MyLang::new())?;
/// # }
/// ```
pub trait Language<C: Clone + PartialEq + Send + 'static>: Send + Sync + 'static {
    /// Whether to advertise and answer `textDocument/documentSymbol`.
    /// Defaults to `false`; flip to `true` and override [`symbols`](Self::symbols).
    ///
    /// For per-instance decisions (e.g. capability discovered from a config
    /// file), override [`supports_symbols`](Self::supports_symbols) instead —
    /// it defaults to this constant.
    const SUPPORTS_SYMBOLS: bool = false;

    /// Runtime hook for symbol support; defaults to
    /// [`SUPPORTS_SYMBOLS`](Self::SUPPORTS_SYMBOLS).
    fn supports_symbols(&self) -> bool {
        Self::SUPPORTS_SYMBOLS
    }

    /// The pass schedule run over every document.
    fn engine(&self) -> &Engine<C>;

    /// Context for a freshly opened document's root region.
    fn root_ctx(&self) -> C;

    /// The position encoding to negotiate with the client.
    /// Defaults to UTF-16, the LSP default and what most clients use.
    fn encoding(&self) -> PositionEncoding {
        PositionEncoding::Utf16
    }

    /// Renders the diagnostic for a failing region, or `None` to stay
    /// silent (e.g. for contexts whose failure is expected).
    ///
    /// `doc` gives access to the document's text, URI, and version; the
    /// returned diagnostic's range may be left empty, in which case it is
    /// filled in from the node's span.
    fn diagnostic(
        &self,
        doc: &Document<C>,
        node: FailedNode<'_, C>,
    ) -> Option<lsp_types::Diagnostic>;

    /// Diagnostics that do not come from parse failures — lint-rule
    /// violations, style warnings, anything computed from the settled
    /// document rather than a failing tree node. Merged into the same
    /// `publishDiagnostics` notification, after the parse diagnostics.
    fn extra_diagnostics(&self, doc: &Document<C>) -> Vec<lsp_types::Diagnostic> {
        let _ = doc;
        Vec::new()
    }

    /// The document symbols for the outline view. Only consulted when
    /// [`supports_symbols`](Self::supports_symbols) is `true`.
    fn symbols(&self, doc: &Document<C>) -> Vec<lsp_types::DocumentSymbol> {
        let _ = doc;
        Vec::new()
    }

    /// Runtime hook for hover support; defaults to `false`.
    fn supports_hover(&self) -> bool {
        false
    }

    /// Runtime hook for go-to-definition support; defaults to `false`.
    fn supports_definition(&self) -> bool {
        false
    }

    /// Runtime hook for completion support; defaults to `false`.
    fn supports_completion(&self) -> bool {
        false
    }

    /// Runtime hook for code-action support (the editor's quickfix
    /// lightbulb); defaults to `false`.
    fn supports_code_actions(&self) -> bool {
        false
    }

    /// Code actions for the given range — typically quickfixes for the
    /// diagnostics published there. The skeleton dispatches
    /// `textDocument/codeAction` when
    /// [`supports_code_actions`](Self::supports_code_actions) is `true`.
    fn code_action(
        &self,
        doc: &Document<C>,
        range: lsp_types::Range,
    ) -> Vec<lsp_types::CodeAction> {
        let _ = (doc, range);
        Vec::new()
    }

    /// Runtime hook for folding-range support; defaults to `false`.
    ///
    /// Folding ranges map parse-tree regions onto foldable sections in
    /// the editor — the most natural LSP feature for an engine whose
    /// whole model is a region tree.
    fn supports_folding_ranges(&self) -> bool {
        false
    }

    /// Folding ranges for the document — one per region the language
    /// server wants collapsible.
    fn folding_ranges(&self, doc: &Document<C>) -> Vec<lsp_types::FoldingRange> {
        let _ = doc;
        Vec::new()
    }

    /// Runtime hook for document-highlight support; defaults to `false`.
    ///
    /// Highlights all occurrences of the token at the cursor — useful
    /// for showing where a variable, class, or rule applies.
    fn supports_document_highlight(&self) -> bool {
        false
    }

    /// Document highlights for the token at the cursor position.
    fn document_highlight(
        &self,
        doc: &Document<C>,
        offset: usize,
    ) -> Vec<lsp_types::DocumentHighlight> {
        let _ = (doc, offset);
        Vec::new()
    }

    /// Hover contents for the byte `offset` in `doc`, or `None`.
    ///
    /// The skeleton converts the client's position (in the negotiated
    /// encoding) to a byte offset before calling this, so implementations
    /// never touch position math.
    fn hover(&self, doc: &Document<C>, offset: usize) -> Option<lsp_types::Hover> {
        let _ = (doc, offset);
        None
    }

    /// Definition locations for the byte `offset` in `doc`, or `None`.
    fn definition(&self, doc: &Document<C>, offset: usize) -> Option<Vec<Location>> {
        let _ = (doc, offset);
        None
    }

    /// Completions for the byte `offset` in `doc`, or `None`.
    fn completion(&self, doc: &Document<C>, offset: usize) -> Option<CompletionResponse> {
        let _ = (doc, offset);
        None
    }
}

/// The server capabilities advertised for `language`.
pub(crate) fn capabilities<C, L>(language: &L) -> lsp_types::ServerCapabilities
where
    C: Clone + PartialEq + Send + 'static,
    L: Language<C>,
{
    lsp_types::ServerCapabilities {
        position_encoding: Some(language.encoding().capability()),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        document_symbol_provider: Some(OneOf::Left(language.supports_symbols())),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(
            language.supports_hover(),
        )),
        definition_provider: Some(OneOf::Left(language.supports_definition())),
        completion_provider: language
            .supports_completion()
            .then(|| lsp_types::CompletionOptions {
                ..Default::default()
            }),
        folding_range_provider: Some(lsp_types::FoldingRangeProviderCapability::Simple(
            language.supports_folding_ranges(),
        )),
        document_highlight_provider: Some(OneOf::Left(language.supports_document_highlight())),
        code_action_provider: Some(lsp_types::CodeActionProviderCapability::Simple(
            language.supports_code_actions(),
        )),
        ..Default::default()
    }
}

fn publish<C, L>(
    connection: &Connection,
    language: &L,
    doc: &Document<C>,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
    C: Clone + PartialEq + Send + 'static,
    L: Language<C>,
{
    let mut diags = diagnostics::diagnostics(doc, DiagnosticsOptions::default(), |node| {
        language.diagnostic(doc, node)
    });
    diags.extend(language.extra_diagnostics(doc));
    let params = lsp_types::PublishDiagnosticsParams {
        uri: doc.uri().clone(),
        diagnostics: diags,
        version: Some(doc.version()),
    };
    connection.sender.send(lsp_server::Message::Notification(
        lsp_server::Notification::new("textDocument/publishDiagnostics".into(), params),
    ))?;
    Ok(())
}

/// Runs a full server lifecycle over an existing `connection`:
/// the `initialize` handshake, then the message loop until `exit`.
///
/// Use this instead of [`serve`] when you own the transport (TCP, an
/// in-process [`Connection::memory`] pair, tests). The caller is responsible
/// for any I/O threads; `serve_on` returns once the client sends `shutdown`
/// + `exit`.
#[allow(clippy::mutable_key_type)]
pub fn serve_on<C, L>(
    connection: Connection,
    language: L,
    documents: Documents<C>,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
    C: Clone + PartialEq + Send + 'static,
    L: Language<C>,
{
    #[allow(clippy::mutable_key_type)]
    let mut documents = documents;
    let _initialization_params =
        connection.initialize(serde_json::to_value(capabilities(&language))?)?;

    run_loop(connection, language, &mut documents)
}

#[allow(clippy::mutable_key_type)]
fn run_loop<C, L>(
    connection: Connection,
    language: L,
    documents: &mut Documents<C>,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
    C: Clone + PartialEq + Send + 'static,
    L: Language<C>,
{
    for msg in &connection.receiver {
        match msg {
            lsp_server::Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    break;
                }
                match req.method.as_str() {
                    "textDocument/documentSymbol" if language.supports_symbols() => {
                        let params: lsp_types::DocumentSymbolParams =
                            serde_json::from_value(req.params)?;
                        let symbols = documents
                            .get(&params.text_document.uri)
                            .map(|doc| language.symbols(doc))
                            .unwrap_or_default();
                        connection.sender.send(lsp_server::Message::Response(
                            lsp_server::Response::new_ok(req.id, symbols),
                        ))?;
                    }
                    "textDocument/hover" if language.supports_hover() => {
                        let params: HoverParams = serde_json::from_value(req.params)?;
                        let tdp = &params.text_document_position_params;
                        let hover = documents
                            .get(&tdp.text_document.uri)
                            .and_then(|doc| language.hover(doc, doc.offset(tdp.position)));
                        connection.sender.send(lsp_server::Message::Response(
                            lsp_server::Response::new_ok(req.id, hover),
                        ))?;
                    }
                    "textDocument/definition" if language.supports_definition() => {
                        let params: lsp_types::GotoDefinitionParams =
                            serde_json::from_value(req.params)?;
                        let tdp = &params.text_document_position_params;
                        let locations = documents
                            .get(&tdp.text_document.uri)
                            .and_then(|doc| language.definition(doc, doc.offset(tdp.position)));
                        connection.sender.send(lsp_server::Message::Response(
                            lsp_server::Response::new_ok(req.id, locations),
                        ))?;
                    }
                    "textDocument/completion" if language.supports_completion() => {
                        let params: CompletionParams = serde_json::from_value(req.params)?;
                        let tdp = &params.text_document_position;
                        let completions = documents
                            .get(&tdp.text_document.uri)
                            .and_then(|doc| language.completion(doc, doc.offset(tdp.position)));
                        connection.sender.send(lsp_server::Message::Response(
                            lsp_server::Response::new_ok(req.id, completions),
                        ))?;
                    }
                    "textDocument/foldingRange" if language.supports_folding_ranges() => {
                        let params: lsp_types::FoldingRangeParams =
                            serde_json::from_value(req.params)?;
                        let ranges = documents
                            .get(&params.text_document.uri)
                            .map(|doc| language.folding_ranges(doc))
                            .unwrap_or_default();
                        connection.sender.send(lsp_server::Message::Response(
                            lsp_server::Response::new_ok(req.id, ranges),
                        ))?;
                    }
                    "textDocument/documentHighlight" if language.supports_document_highlight() => {
                        let params: lsp_types::DocumentHighlightParams =
                            serde_json::from_value(req.params)?;
                        let tdp = &params.text_document_position_params;
                        let highlights = documents
                            .get(&tdp.text_document.uri)
                            .map(|doc| language.document_highlight(doc, doc.offset(tdp.position)))
                            .unwrap_or_default();
                        connection.sender.send(lsp_server::Message::Response(
                            lsp_server::Response::new_ok(req.id, highlights),
                        ))?;
                    }
                    "textDocument/codeAction" if language.supports_code_actions() => {
                        let params: CodeActionParams = serde_json::from_value(req.params)?;
                        let actions = documents
                            .get(&params.text_document.uri)
                            .map(|doc| language.code_action(doc, params.range))
                            .unwrap_or_default();
                        connection.sender.send(lsp_server::Message::Response(
                            lsp_server::Response::new_ok(
                                req.id,
                                actions
                                    .into_iter()
                                    .map(lsp_types::CodeActionOrCommand::CodeAction)
                                    .collect::<Vec<_>>(),
                            ),
                        ))?;
                    }
                    _ => {
                        connection.sender.send(lsp_server::Message::Response(
                            lsp_server::Response::new_err(
                                req.id,
                                lsp_server::ErrorCode::MethodNotFound as i32,
                                "method not supported".into(),
                            ),
                        ))?;
                    }
                }
            }
            lsp_server::Message::Notification(notification) => {
                let lsp_server::Notification { method, params, .. } = notification;
                match method.as_str() {
                    "textDocument/didOpen" => {
                        let params: DidOpenTextDocumentParams = serde_json::from_value(params)?;
                        let item = params.text_document;
                        let mut doc = Document::open(
                            item.uri.clone(),
                            item.version,
                            item.language_id.clone(),
                            item.text,
                            language.encoding(),
                            language.root_ctx(),
                        );
                        doc.apply_changes(
                            language.engine(),
                            item.version,
                            &[],
                            &increparse::SerialExecutor,
                            &CancelToken::new(),
                        );
                        publish(&connection, &language, &doc)?;
                        documents.insert(item.uri, doc);
                    }
                    "textDocument/didChange" => {
                        let params: DidChangeTextDocumentParams = serde_json::from_value(params)?;
                        let uri = params.text_document.uri.clone();
                        if let Some(doc) = documents.get_mut(&uri) {
                            doc.apply_changes(
                                language.engine(),
                                params.text_document.version,
                                &params.content_changes,
                                &increparse::SerialExecutor,
                                &CancelToken::new(),
                            );
                            publish(&connection, &language, doc)?;
                        }
                    }
                    "textDocument/didClose" => {
                        let params: DidCloseTextDocumentParams = serde_json::from_value(params)?;
                        let uri = params.text_document.uri;
                        documents.remove(&uri);
                        connection.sender.send(lsp_server::Message::Notification(
                            lsp_server::Notification::new(
                                "textDocument/publishDiagnostics".into(),
                                lsp_types::PublishDiagnosticsParams {
                                    uri,
                                    diagnostics: Vec::new(),
                                    version: None,
                                },
                            ),
                        ))?;
                    }
                    _ => {}
                }
            }
            lsp_server::Message::Response(_) => {}
        }
    }

    Ok(())
}

/// Runs a language server on stdio until the client sends `shutdown` +
/// `exit`.
///
/// This is the whole integration point:
///
/// ```no_run
/// # use increparse::{Engine, Outcome, Pass, Schedule, Span};
/// # use increparse_lsp::{Document, FailedNode, Language};
/// # use lsp_types::Diagnostic;
/// # #[derive(Clone, Debug, PartialEq, Eq)]
/// # enum Ctx { File }
/// # struct Accept;
/// # impl Pass for Accept {
/// #     type Ctx = Ctx;
/// #     fn parse(&self, _source: &str, _span: Span, _ctx: &Ctx) -> Outcome<Ctx> {
/// #         Outcome::Done
/// #     }
/// # }
/// # struct MyLang { engine: Engine<Ctx> }
/// # impl Language<Ctx> for MyLang {
/// #     fn engine(&self) -> &Engine<Ctx> { &self.engine }
/// #     fn root_ctx(&self) -> Ctx { Ctx::File }
/// #     fn diagnostic(&self, _doc: &Document<Ctx>, _node: FailedNode<'_, Ctx>) -> Option<Diagnostic> { None }
/// # }
/// # fn build_engine() -> Engine<Ctx> { unimplemented!() }
/// fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
///     increparse_lsp::serve(MyLang { engine: build_engine() })
/// }
/// ```
pub fn serve<C, L>(language: L) -> Result<(), Box<dyn Error + Send + Sync>>
where
    C: Clone + PartialEq + Send + 'static,
    L: Language<C>,
{
    let (connection, io_threads) = Connection::stdio();
    serve_on(connection, language, Documents::new())?;
    io_threads.join()?;
    Ok(())
}
