use ginko::dts::{
    AnyDirective, Bindings, CompletionContext, FileType, HasSpan, IncludeLoader,
    IncludeLoaderGuard, IncludeLoaderNotifyAction, ItemAtCursor, Node, NodeItem, NodePayload,
    ParserConfig, Primary, Project, ResolvedContext, ResolvedNode, ResolvedProperty, Severity,
    SeverityMap, Span, node_compatibles,
};
use ginko::header::{Define, TokenKind, parse_header};
use itertools::Itertools;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::GotoImplementationParams;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use url::Url;

pub struct Backend {
    name: String,
    client: Client,
    project: RwLock<Project>,
    severities: SeverityMap,
    loader: RwLock<IncludeLoaderGuard>,
    bindings: Bindings,
    include_paths: Vec<PathBuf>,
    header_define_cache: RwLock<HashMap<PathBuf, Arc<Vec<Define>>>>,
}

impl Backend {
    pub fn new(name: &str, client: Client) -> Backend {
        Backend {
            name: name.to_string(),
            client,
            project: RwLock::new(Project::default()),
            severities: SeverityMap::default(),
            loader: RwLock::new(IncludeLoaderGuard::default()),
            bindings: Bindings::default(),
            include_paths: Vec::new(),
            header_define_cache: RwLock::new(HashMap::new()),
        }
    }
}

/// This is the shape of the JSON object we receive from the client
/// in the `did_change_configuration` callback. This is a free-form
/// object in LSP; we're choosing to receive the root DTS file here.
#[derive(Deserialize, Serialize, Default, Debug)]
struct ProjectConfig {
    pub target: String,
}

impl ProjectConfig {
    pub fn from_value(value: Value) -> Self {
        serde_json::from_value(value).unwrap_or_default()
    }
}

fn lsp_severity_from_severity(severity_level: Severity) -> DiagnosticSeverity {
    match severity_level {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Hint => DiagnosticSeverity::HINT,
    }
}

fn lsp_range_from_span(span: Span) -> Range {
    Range::new(lsp_pos_from_pos(span.start()), lsp_pos_from_pos(span.end()))
}

fn lsp_pos_from_pos(pos: ginko::dts::Position) -> Position {
    Position::new(pos.line(), pos.character())
}

impl Backend {
    pub fn with_loader<L>(mut self, loader: L) -> Self
    where
        L: IncludeLoader + Send + 'static,
    {
        self.loader = RwLock::new(IncludeLoaderGuard::new(loader));
        self
    }

    /// Configure the include search paths (the `-i/--include` values passed
    /// to the binary). Without this, CLI-provided paths never reach the
    /// project's include loader.
    pub fn with_include_paths(mut self, include_paths: Vec<String>) -> Self {
        self.loader.write().set_include_paths(include_paths.clone());
        self.include_paths = include_paths.iter().map(PathBuf::from).collect();
        self
    }

    /// Configure the binding directories (the `-b/--bindings` values passed
    /// to the binary) powering binding-based completion.
    pub fn with_bindings_paths(mut self, binding_paths: Vec<String>) -> Self {
        let paths: Vec<PathBuf> = binding_paths.iter().map(PathBuf::from).collect();
        self.bindings = Bindings::from_paths(&paths);
        tracing::info!(
            "bindings: indexed {} schemas over {} compatible strings",
            self.bindings.schema_count(),
            self.bindings.compatible_count()
        );
        self
    }

    /// Resolve a relative header path named by a binding description (e.g.
    /// `include/dt-bindings/leds/common.h`) under the configured include
    /// paths and scan it once for `#define`s. The scanned defines are cached
    /// per resolved file for the session.
    fn header_defines(&self, relative: &str) -> Option<Arc<Vec<Define>>> {
        let resolved = self.include_paths.iter().find_map(|root| {
            let direct = root.join(relative);
            let stripped = relative
                .strip_prefix("include/")
                .map(|rest| root.join(rest));
            std::iter::once(&direct)
                .chain(stripped.iter())
                .find(|candidate| candidate.exists())
                .cloned()
        })?;
        if let Some(defines) = self.header_define_cache.read().get(&resolved) {
            return Some(defines.clone());
        }
        let text = fs::read_to_string(&resolved).ok()?;
        let source = Arc::from(resolved.as_path());
        let defines = Arc::new(parse_header(&text, source).defines);
        self.header_define_cache
            .write()
            .insert(resolved, defines.clone());
        Some(defines)
    }

    pub fn with_parser_config(self, config: ParserConfig) -> Self {
        self.project.write().set_parser_config(config);
        self
    }

    pub fn with_canonicalizer(self, f: fn(&Path) -> std::io::Result<PathBuf>) -> Self {
        self.project.write().set_canonicalizer(f);
        self
    }

    fn lsp_diag_from_diag(&self, diagnostic: &ginko::dts::Diagnostic) -> Diagnostic {
        let span = diagnostic.span();
        Diagnostic {
            range: lsp_range_from_span(span),
            message: diagnostic.message.clone(),
            code: Some(NumberOrString::String(diagnostic.kind.as_ref().to_string())),
            severity: Some(lsp_severity_from_severity(
                diagnostic.severity(&self.severities),
            )),
            source: Some(self.name.clone()),
            ..Default::default()
        }
    }

    async fn url_to_file_path(&self, url: &Url) -> Option<PathBuf> {
        match url.to_file_path() {
            Ok(path) => Some(path),
            Err(_) => {
                self.client
                    .show_message(
                        MessageType::ERROR,
                        format!("Url {url} is not a valid file path"),
                    )
                    .await;
                None
            }
        }
    }

    async fn publish_diagnostics(&self) {
        let file_paths = self
            .project
            .read()
            .files()
            .map(|file| file.to_owned())
            .collect_vec();
        for file in file_paths {
            let diagnostics = self
                .project
                .read()
                .get_diagnostics(&file)
                .map(|diag| self.lsp_diag_from_diag(diag))
                .collect_vec();
            self.client
                .publish_diagnostics(Url::from_file_path(&file).unwrap(), diagnostics, None)
                .await
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        // If we wanted, we could pass arbitrary data from the client
        // into this function as params.initialization_options.
        // But we are getting a `did_change_configuration` on start,
        // so we can handle the initial configuration from there.

        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec!["&".to_string(), "/".to_string()]),
                    all_commit_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                    completion_item: None,
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let mut changed = false;
        for change in params.changes {
            let Some(file_path) = self.url_to_file_path(&change.uri).await else {
                continue;
            };

            let mut loader = self.loader.write();
            match loader.notify(Path::new(file_path.as_path())) {
                IncludeLoaderNotifyAction::None => {}
                IncludeLoaderNotifyAction::Reset => {
                    let _ = self.project.write().reset(&mut loader);
                    changed = true;
                }
            }
        }
        if changed {
            self.publish_diagnostics().await;
            if let Err(err) = self.client.workspace_diagnostic_refresh().await {
                eprintln!("Error reporting workspace diagnostic refresh: {err}");
            }
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let config = ProjectConfig::from_value(params.settings);
        {
            let mut loader = self.loader.write();
            let _ = self
                .project
                .write()
                .set_root_file_and_reset(config.target, &mut loader);
        }
        self.publish_diagnostics().await;
    }

    async fn initialized(&self, _: InitializedParams) {
        let Some(watchers) = self.loader.read().watch_patterns().map(|patterns| {
            patterns
                .into_iter()
                .map(|pattern| FileSystemWatcher {
                    glob_pattern: GlobPattern::String(pattern),
                    kind: None, // default: Create | Change | Delete
                })
                .collect::<Vec<_>>()
        }) else {
            return;
        };

        let _ = self
            .client
            .register_capability(vec![Registration {
                id: "watch-patterns".to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers,
                })
                .ok(),
            }])
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let Some(file_path) = self
            .url_to_file_path(&params.text_document.uri)
            .await
        else {
            return;
        };
        let file_type = FileType::from(file_path.as_path());
        self.project.write().add_file_with_text(
            file_path.clone(),
            params.text_document.text,
            file_type,
            &mut self.loader.write(),
        );
        self.publish_diagnostics().await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(file_path) = self.url_to_file_path(&params.text_document.uri).await else {
            return;
        };
        let file_type = FileType::from(file_path.as_path());
        self.project.write().add_file_with_text(
            file_path.clone(),
            params.content_changes.into_iter().next().unwrap().text,
            file_type,
            &mut self.loader.write(),
        );
        self.publish_diagnostics().await
    }

    async fn did_save(&self, _: DidSaveTextDocumentParams) {}

    async fn did_close(&self, _: DidCloseTextDocumentParams) {}

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        fn completions_for_reference(
            project: &Project,
            file_path: &Path,
            token: &str,
        ) -> Option<CompletionResponse> {
            let analysis = project.get_analysis(file_path)?;
            Some(CompletionResponse::Array(
                analysis
                    .get_labels()
                    .into_iter()
                    .filter(|label| label.starts_with(token))
                    .map(|label| {
                        let detail = analysis.get_referred_by_label(&label);
                        CompletionItem {
                            label,
                            detail,
                            ..Default::default()
                        }
                    })
                    .collect(),
            ))
        }

        fn completions_for_referenced_node(
            project: &Project,
            file_path: &Path,
            token: &str,
        ) -> Option<CompletionResponse> {
            let analysis = project.get_analysis(file_path)?;
            Some(CompletionResponse::Array(
                analysis
                    .get_labels()
                    .into_iter()
                    .filter(|label| label.starts_with(token))
                    .filter_map(|label| {
                        if let Some(detail) = analysis.get_referred_by_label(&label) {
                            let snippet = format!("{label} {{\n\t$0\n}};");
                            Some(CompletionItem {
                                label,
                                detail: Some(detail),
                                kind: Some(CompletionItemKind::MODULE),
                                insert_text: Some(snippet),
                                insert_text_format: Some(InsertTextFormat::SNIPPET),
                                ..Default::default()
                            })
                        } else {
                            None
                        }
                    })
                    .collect(),
            ))
        }

        fn completions_for_node(
            rnode: &ResolvedNode,
            bindings: &Bindings,
        ) -> Option<CompletionResponse> {
            let mut items = vec![];
            for name in rnode.active_child_nodes() {
                let snippet = format!("{name} {{\n\t$0\n}};");
                items.push(CompletionItem {
                    label: name,
                    detail: Some("node".to_string()),
                    kind: Some(CompletionItemKind::MODULE),
                    insert_text: Some(snippet),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
            }
            for (name, prop) in rnode.active_child_properties() {
                let snippet = if prop.values.is_empty() {
                    format!("{name};")
                } else {
                    format!("{name} = $0{};", prop.display_values())
                };
                items.push(CompletionItem {
                    label: name,
                    detail: Some("property".to_string()),
                    insert_text: Some(snippet),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
            }
            // Binding-derived property suggestions: the node's compatibles
            // (overlay-merged, so `&ref { }` blocks see the base dtsi's
            // compatible) map to binding schemas with the allowed properties.
            for (name, hint) in bindings.properties_for(&node_compatibles(rnode)) {
                if items.iter().any(|item| item.label == name) {
                    continue;
                }
                let snippet = if hint.boolean {
                    format!("{name};")
                } else {
                    format!("{name} = $0;")
                };
                let detail = if hint.required {
                    "required property".to_string()
                } else {
                    hint.type_hint.unwrap_or_else(|| "property".to_string())
                };
                let mut item = CompletionItem {
                    label: name,
                    detail: Some(detail),
                    kind: Some(CompletionItemKind::FIELD),
                    insert_text: Some(snippet),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                };
                if let Some(description) = hint.description {
                    item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: description,
                    }));
                }
                items.push(item);
            }
            Some(CompletionResponse::Array(items))
        }

        fn completions_for_property_values(
            this: &Backend,
            project: &Project,
            file_path: &Path,
            rnode: &ResolvedNode,
            property: &ResolvedProperty,
            bindings: &Bindings,
        ) -> Option<CompletionResponse> {
            let name = property.property().name.item();
            let properties = bindings.properties_for(&node_compatibles(rnode));
            let matched = properties.get(name);
            let yaml_values: &[String] = match matched.map(|hint| &hint.values) {
                Some(values) if !values.is_empty() => values,
                _ => bindings.common_values(name),
            };
            let mut items: Vec<CompletionItem> = yaml_values
                .iter()
                .map(|value| CompletionItem {
                    label: value.clone(),
                    detail: Some(format!("{name} value")),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    ..Default::default()
                })
                .collect();
            let mut define_prefixes: BTreeMap<&str, &str> = BTreeMap::new();
            for hint in matched.into_iter().chain(bindings.common_hint(name)) {
                for (prefix, header) in &hint.define_families {
                    define_prefixes
                        .entry(prefix.as_str())
                        .or_insert(header.as_str());
                }
            }
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for (prefix, header) in define_prefixes {
                let Some(defines) = this.header_defines(header) else {
                    continue;
                };
                // A representative candidate decides whether the family's
                // defines are already in scope for this file's include
                // chain; if not, accepting any item of the family also adds
                // the binding-named #include.
                let representative = defines.iter().find_map(|define| {
                    let TokenKind::Ident(label) = &define.name.kind else {
                        return None;
                    };
                    (!define.fn_like && label.starts_with(prefix)).then(|| label.clone())
                });
                let include_edit = representative.and_then(|representative| {
                    additional_include_edit(
                        project,
                        file_path,
                        &representative,
                        &dts_include_path(header),
                    )
                });
                for define in defines.iter() {
                    if define.fn_like {
                        continue;
                    }
                    let TokenKind::Ident(label) = &define.name.kind else {
                        continue;
                    };
                    if !label.starts_with(prefix) || !seen.insert(label.clone()) {
                        continue;
                    }
                    items.push(CompletionItem {
                        label: label.clone(),
                        detail: Some(format!("{} ({header})", define.body)),
                        kind: Some(CompletionItemKind::CONSTANT),
                        additional_text_edits: include_edit.clone().map(|edit| vec![edit]),
                        ..Default::default()
                    });
                }
            }
            (!items.is_empty()).then_some(CompletionResponse::Array(items))
        }

        /// The DTS spelling of a binding-relative header path: strip the
        /// leading `include/` — `#include <dt-bindings/…>` resolves against
        /// the `--include` roots.
        fn dts_include_path(relative: &str) -> String {
            relative
                .strip_prefix("include/")
            .unwrap_or(relative)
                .to_string()
        }

        /// The `#include <…>` TextEdit to attach to header-define completion
        /// items when the family's defines are not yet in scope for the
        /// completed file. None when an already-included header (directly or
        /// transitively) provides the defines.
        fn additional_include_edit(
            project: &Project,
            file_path: &Path,
            representative_define: &str,
            include_path: &str,
        ) -> Option<TextEdit> {
            if project
                .get_analysis(file_path)
                .and_then(|analysis| analysis.get_define(representative_define))
                .is_some()
            {
                return None;
            }
            let source = project.file_source(file_path)?;
            let line = include_insert_line(source);
            Some(TextEdit {
                range: Range::new(Position::new(line, 0), Position::new(line, 0)),
                new_text: format!("#include <{include_path}>\n"),
            })
        }

        /// Insert the new `#include` after the last existing one, else after
        /// the `/dts-v1/;` directive, else at the top of the file.
        fn include_insert_line(source: &str) -> u32 {
            let mut last_include: Option<u32> = None;
            let mut dts_v1_line: Option<u32> = None;
            for (i, line) in source.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("#include") {
                    last_include = Some(i as u32);
                } else if dts_v1_line.is_none() && trimmed.starts_with("/dts-v1/") {
                    dts_v1_line = Some(i as u32);
                }
            }
            last_include
                .or(dts_v1_line)
                .map(|line| line + 1)
                .unwrap_or(0)
        }

        fn property_value_completion(
            this: &Backend,
            project: &Project,
            file_path: &Path,
            pos: &ginko::dts::Position,
        ) -> Option<CompletionResponse> {
            match project.completion_context_at(file_path, pos)? {
                CompletionContext::InProperty(rnode, property)
                    if *pos >= property.property().name.span().end() =>
                {
                    completions_for_property_values(
                        this,
                        project,
                        file_path,
                        rnode,
                        property,
                        &this.bindings,
                    )
                }
                _ => None,
            }
        }

        fn completions_for_root() -> Option<CompletionResponse> {
            let mut items = vec![];
            let snippet = " {\n\t$0\n};".to_string();
            items.push(CompletionItem {
                label: "/".to_string(),
                detail: Some("node".to_string()),
                kind: Some(CompletionItemKind::MODULE),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
            Some(CompletionResponse::Array(items))
        }

        let Some(file_path) = self
            .url_to_file_path(&params.text_document_position.text_document.uri)
            .await
        else {
            return Ok(None);
        };

        let pos = position_to_ginko_position(params.text_document_position.position);
        let project = self.project.read();
        let Some(token) = project.token_at(&file_path, &pos) else {
            return Ok(None);
        };

        // Check if cursor is on a "node context" (e.g. inside node brackets),
        // or not (e.g. at the toplevel).
        match project.context_at(&file_path, &pos) {
            Some(ResolvedContext::Node(rnode)) => {
                if let Some(token) = token.strip_prefix('&') {
                    return Ok(completions_for_reference(&project, &file_path, token));
                }

                return Ok(completions_for_node(rnode, &self.bindings));
            }
            Some(ResolvedContext::Property(_)) => {
                if let Some(token) = token.strip_prefix('&') {
                    return Ok(completions_for_reference(&project, &file_path, token));
                }
                // Positions inside a property span resolve to `Property` here;
                // binding-aware value completion rides the span-based context.
                if let Some(response) =
                    property_value_completion(self, &project, &file_path, &pos)
                {
                    return Ok(Some(response));
                }
            }
            None => {
                if let Some(token) = token.strip_prefix('&') {
                    return Ok(completions_for_referenced_node(&project, &file_path, token));
                }

                if let Some(context) = params.context {
                    if context.trigger_kind == CompletionTriggerKind::TRIGGER_CHARACTER
                        && context.trigger_character == Some("/".to_string())
                        && params.text_document_position.position.character == 1
                    {
                        return Ok(completions_for_root());
                    }
                }

                // No `&ref` context — fall back to span-based resolution of
                // the node the cursor is inside (mid-payload completion).
                match project.completion_context_at(&file_path, &pos) {
                    Some(CompletionContext::InNode(rnode)) => {
                        return Ok(completions_for_node(rnode, &self.bindings))
                    }
                    // Property-span positions that the reference-based
                    // context missed (e.g. inside values) complete values
                    // once past the property's name.
                    Some(CompletionContext::InProperty(..)) => {
                        if let Some(response) =
                            property_value_completion(self, &project, &file_path, &pos)
                        {
                            return Ok(Some(response));
                        }
                    }
                    Some(CompletionContext::TopLevel) | None => {}
                }
            }
        }

        Ok(None)
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let Some(file_path) = self
            .url_to_file_path(&params.text_document_position_params.text_document.uri)
            .await
        else {
            return Ok(None);
        };
        let pos = position_to_ginko_position(params.text_document_position_params.position);
        let project = self.project.read();

        match project.context_at(&file_path, &pos) {
            Some(ResolvedContext::Node(rnode)) => {
                return Ok(Some(GotoDefinitionResponse::Array(
                    rnode
                        .sources()
                        .map(|loc| {
                            Location::new(
                                Url::from_file_path(loc.file()).unwrap(),
                                ginko_span_to_range(loc.span()),
                            )
                        })
                        .collect(),
                )));
            }
            Some(ResolvedContext::Property(_)) => Ok(None),
            None => Ok(None),
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let Some(file_path) = self
            .url_to_file_path(&params.text_document_position_params.text_document.uri)
            .await
        else {
            return Ok(None);
        };
        let pos = position_to_ginko_position(params.text_document_position_params.position);
        let project = self.project.read();
        let Some(item) = project.find_at_pos(&file_path, &pos) else {
            return Ok(None);
        };
        match item {
            ItemAtCursor::Reference(reference, ctx) => {
                match project.get_node_position(&file_path, reference, &ctx) {
                    Some((span, path)) => Ok(Some(GotoDefinitionResponse::Scalar(Location::new(
                        Url::from_file_path(path).unwrap(),
                        ginko_span_to_range(span),
                    )))),
                    None => Ok(None),
                }
            }
            ItemAtCursor::Include(include) => {
                match self
                    .loader
                    .write()
                    .load(&file_path, &include.file_name(), include.kind())
                    .ok()
                    .and_then(|path| Url::from_file_path(path).ok())
                {
                    Some(url) => Ok(Some(GotoDefinitionResponse::Scalar(Location::new(
                        url,
                        Range::new(Position::new(0, 0), Position::new(0, 0)),
                    )))),
                    _ => Ok(None),
                }
            }
            ItemAtCursor::Macro(token) => {
                let Some(define) = project
                    .get_analysis(&file_path)
                    .and_then(|ctx| ctx.get_define(token.item()))
                else {
                    return Ok(None);
                };

                Ok(Some(GotoDefinitionResponse::Scalar(Location::new(
                    Url::from_file_path(&define.name.source).unwrap(),
                    ginko_span_to_range(define.name.span()),
                ))))
            }
            _ => Ok(None),
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let Some(file_path) = self
            .url_to_file_path(&params.text_document_position_params.text_document.uri)
            .await
        else {
            return Ok(None);
        };
        let pos = position_to_ginko_position(params.text_document_position_params.position);
        let project = self.project.read();
        let Some(item) = project.find_at_pos(&file_path, &pos) else {
            return Ok(None);
        };

        let Some(str) = (match item {
            ItemAtCursor::Node(node) if node.span().start().line() == pos.line() => {
                context_based_hover(&project, &file_path, &pos)
            }
            ItemAtCursor::ReferencedNode(rnode) if rnode.span().start().line() == pos.line() => {
                context_based_hover(&project, &file_path, &pos)
            }
            ItemAtCursor::Reference(reference, ctx) => {
                project.document_reference(&file_path, reference, &ctx)
            }
            ItemAtCursor::Label(name) => Some(name.item().clone()),
            ItemAtCursor::Include(include) => Some(include.file_name.item().clone().path),
            ItemAtCursor::Macro(token) => 'block: {
                let Some(ctx) = project.get_analysis(&file_path) else {
                    break 'block None;
                };
                let Some(define) = ctx.get_define(token.item()) else {
                    break 'block None;
                };
                let base = format!("#define {} {}", token.item(), define.body);

                match ctx.resolve_value(token) {
                    Some(resolved) if resolved != define.body => {
                        Some(format!("{}\n{}", base, resolved))
                    }
                    Some(_) | None => Some(base),
                }
            }

            _ => None,
        }) else {
            return Ok(None);
        };

        Ok(Some(Hover {
            contents: HoverContents::Scalar(MarkedString::LanguageString(LanguageString {
                language: "dts".to_string(),
                value: str.to_string(),
            })),
            range: None,
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(file_path) = self.url_to_file_path(&params.text_document.uri).await else {
            return Ok(None);
        };
        let project = self.project.read();
        let Some(root) = project.get_root(&file_path) else {
            return Ok(None);
        };
        #[allow(deprecated)]
        fn node_payload_to_symbol(payload: &NodePayload) -> Vec<DocumentSymbol> {
            let mut children: Vec<DocumentSymbol> = Vec::new();
            for el in &payload.items {
                let document_symbol = match el {
                    NodeItem::Property(prop) => DocumentSymbol {
                        name: prop.name.item().clone(),
                        detail: None,
                        kind: SymbolKind::PROPERTY,
                        tags: None,
                        deprecated: None,
                        range: ginko_span_to_range(prop.span()),
                        selection_range: ginko_span_to_range(prop.name.span()),
                        children: None,
                    },
                    NodeItem::Node(node) => node_to_symbol(node),
                    NodeItem::DeletedNode(start_tok, deleted_node) => DocumentSymbol {
                        name: format!("{}", deleted_node.item()),
                        detail: None,
                        kind: SymbolKind::MODULE,
                        tags: Some(vec![SymbolTag::DEPRECATED]),
                        deprecated: None,
                        range: ginko_span_to_range(Span::new(
                            start_tok.start(),
                            deleted_node.end(),
                        )),
                        selection_range: ginko_span_to_range(deleted_node.span()),
                        children: None,
                    },
                    NodeItem::DeletedProperty(start_tok, deleted_property) => DocumentSymbol {
                        name: deleted_property.item().to_string(),
                        detail: None,
                        kind: SymbolKind::PROPERTY,
                        tags: Some(vec![SymbolTag::DEPRECATED]),
                        deprecated: None,
                        range: ginko_span_to_range(Span::new(
                            start_tok.start(),
                            deleted_property.end(),
                        )),
                        selection_range: ginko_span_to_range(deleted_property.span()),
                        children: None,
                    },
                };
                children.push(document_symbol);
            }
            children
        }
        #[allow(deprecated)]
        fn node_to_symbol(node: &Node) -> DocumentSymbol {
            DocumentSymbol {
                name: node.name.name.clone(),
                detail: None,
                kind: SymbolKind::MODULE,
                tags: None,
                deprecated: None,
                range: ginko_span_to_range(node.span()),
                selection_range: ginko_span_to_range(node.name.span()),
                children: Some(node_payload_to_symbol(&node.payload)),
            }
        }
        #[allow(deprecated)]
        let nodes = root
            .elements
            .iter()
            .filter_map(|el| match el {
                Primary::Root(node) => Some(node_to_symbol(node)),
                Primary::ReferencedNode(ref_node) => Some(DocumentSymbol {
                    name: format!("{}", ref_node.reference),
                    detail: None,
                    kind: SymbolKind::MODULE,
                    tags: None,
                    deprecated: None,
                    range: ginko_span_to_range(ref_node.span()),
                    selection_range: ginko_span_to_range(ref_node.reference.span()),
                    children: Some(node_payload_to_symbol(&ref_node.payload)),
                }),
                Primary::Directive(AnyDirective::Include(include)) => Some(DocumentSymbol {
                    name: format!("include {}", include.file_name.item()),
                    detail: None,
                    kind: SymbolKind::FILE,
                    tags: None,
                    deprecated: None,
                    range: ginko_span_to_range(include.span()),
                    selection_range: ginko_span_to_range(include.file_name.span()),
                    children: None,
                }),
                Primary::Directive(AnyDirective::DeletedNode(token, ref_node)) => {
                    Some(DocumentSymbol {
                        name: format!("{}", ref_node),
                        detail: None,
                        kind: SymbolKind::MODULE,
                        tags: Some(vec![SymbolTag::DEPRECATED]),
                        deprecated: None,
                        range: ginko_span_to_range(Span::new(token.start(), ref_node.end())),
                        selection_range: ginko_span_to_range(ref_node.span()),
                        children: None,
                    })
                }
                Primary::Directive(AnyDirective::OmitIfNoRef(token, ref_node)) => {
                    Some(DocumentSymbol {
                        name: format!("{}", ref_node),
                        detail: None,
                        kind: SymbolKind::FIELD,
                        tags: None,
                        deprecated: None,
                        range: ginko_span_to_range(Span::new(token.start(), ref_node.end())),
                        selection_range: ginko_span_to_range(ref_node.span()),
                        children: None,
                    })
                }
                _ => None,
            })
            .collect_vec();
        Ok(Some(DocumentSymbolResponse::Nested(nodes)))
    }
}

fn ginko_span_to_range(span: Span) -> Range {
    Range::new(
        ginko_position_to_position(span.start()),
        ginko_position_to_position(span.end()),
    )
}

#[allow(unused)]
fn range_to_ginko_span(range: Range) -> Span {
    Span::new(
        position_to_ginko_position(range.start),
        position_to_ginko_position(range.end),
    )
}

fn position_to_ginko_position(position: Position) -> ginko::dts::Position {
    ginko::dts::Position::new(position.line, position.character)
}

fn ginko_position_to_position(position: ginko::dts::Position) -> Position {
    Position::new(position.line(), position.character())
}

fn context_based_hover(
    project: &Project,
    file_path: &Path,
    pos: &ginko::dts::Position,
) -> Option<String> {
    match project.context_at(file_path, pos) {
        Some(ResolvedContext::Node(rnode)) => Some(rnode.to_string()),
        Some(ResolvedContext::Property(_)) => None,
        None => None,
    }
}
