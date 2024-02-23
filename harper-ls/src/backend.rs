use std::{collections::HashMap, sync::Arc};

use harper_core::{
    parsers::Markdown, Dictionary, Document, FullDictionary, LintSet, Linter, MergedDictionary,
};
use itertools::Itertools;
use serde_json::Value;
use tokio::sync::Mutex;
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        notification::{PublishDiagnostics, ShowMessage},
        CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, CodeActionResponse,
        Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, ExecuteCommandParams,
        InitializeParams, InitializeResult, InitializedParams, MessageType,
        PublishDiagnosticsParams, Range, ServerCapabilities, ShowMessageParams,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
        TextDocumentSyncSaveOptions, Url,
    },
    Client, LanguageServer,
};

use crate::{
    config::Config,
    diagnostics::{lint_to_code_actions, lints_to_diagnostics},
    dictionary_io::{load_dict, save_dict},
    pos_conv::range_to_span,
    tree_sitter_parser::TreeSitterParser,
};

#[derive(Default)]
struct DocumentState {
    document: Document,
    ident_dict: Arc<FullDictionary>,
    linter: LintSet,
}

/// Deallocate
pub struct Backend {
    client: Client,
    static_dictionary: Arc<FullDictionary>,
    config: Config,
    doc_state: Mutex<HashMap<Url, DocumentState>>,
}

impl Backend {
    pub fn new(client: Client, config: Config) -> Self {
        let dictionary = FullDictionary::create_from_curated();

        Self {
            client,
            static_dictionary: dictionary.into(),
            doc_state: Mutex::new(HashMap::new()),
            config,
        }
    }

    async fn update_document_from_file(&self, url: &Url) {
        let Ok(content) = tokio::fs::read_to_string(url.path()).await else {
            // TODO: Proper error handling here.
            return;
        };
        self.update_document(url, &content).await;
    }

    async fn load_user_dictionary(&self) -> anyhow::Result<FullDictionary> {
        Ok(match load_dict(&self.config.user_dict_path).await {
            Ok(dict) => dict,
            Err(err) => FullDictionary::new(),
        })
    }

    async fn save_user_dictionary(&self, dict: impl Dictionary) -> anyhow::Result<()> {
        Ok(save_dict(&self.config.user_dict_path, dict).await?)
    }

    async fn generate_global_dictionary(&self) -> anyhow::Result<MergedDictionary<FullDictionary>> {
        let mut dict = MergedDictionary::new();
        dict.add_dictionary(self.static_dictionary.clone());
        let user_dict = self.load_user_dictionary().await?;
        dict.add_dictionary(Arc::new(user_dict));
        Ok(dict)
    }

    async fn update_document(&self, url: &Url, text: &str) -> anyhow::Result<()> {
        let mut lock = self.doc_state.lock().await;
        let doc_state = lock.entry(url.clone()).or_insert(DocumentState {
            linter: LintSet::new().with_standard(self.generate_global_dictionary().await?),
            ..Default::default()
        });

        doc_state.document = if let Some(extension) = url.to_file_path().unwrap().extension() {
            if let Some(ts_parser) =
                TreeSitterParser::new_from_extension(&extension.to_string_lossy())
            {
                let doc = Document::new(text, Box::new(ts_parser.clone()));

                if let Some(new_dict) = ts_parser.create_ident_dict(doc.get_full_content()) {
                    let new_dict = Arc::new(new_dict);

                    if doc_state.ident_dict != new_dict {
                        doc_state.ident_dict = new_dict.clone();
                        let mut merged = self.generate_global_dictionary().await?;
                        merged.add_dictionary(new_dict);

                        doc_state.linter = LintSet::new().with_standard(merged);
                    }
                }

                doc
            } else {
                Document::new(text, Box::new(Markdown))
            }
        } else {
            Document::new(text, Box::new(Markdown))
        };

        Ok(())
    }

    async fn generate_code_actions(
        &self,
        url: &Url,
        range: Range,
    ) -> Result<Vec<CodeActionOrCommand>> {
        let mut doc_states = self.doc_state.lock().await;
        let Some(doc_state) = doc_states.get_mut(url) else {
            return Ok(Vec::new());
        };

        let mut lints = doc_state.linter.lint(&doc_state.document);
        lints.sort_by_key(|l| l.priority);

        let source_chars = doc_state.document.get_full_content();

        // Find lints whose span overlaps with range
        let span = range_to_span(source_chars, range);

        let actions = lints
            .into_iter()
            .filter(|lint| lint.span.overlaps_with(span))
            .flat_map(|lint| lint_to_code_actions(&lint, url, source_chars))
            .collect();

        Ok(actions)
    }

    async fn generate_diagnostics(&self, url: &Url) -> Vec<Diagnostic> {
        let mut doc_states = self.doc_state.lock().await;
        let Some(doc_state) = doc_states.get_mut(url) else {
            return Vec::new();
        };

        let lints = doc_state.linter.lint(&doc_state.document);

        lints_to_diagnostics(doc_state.document.get_full_content(), &lints)
    }

    async fn publish_diagnostics(&self, url: &Url) {
        let client = self.client.clone();

        tokio::spawn(async move {
            client
                .send_notification::<ShowMessage>(ShowMessageParams {
                    typ: MessageType::INFO,
                    message: "Linting...".to_string(),
                })
                .await
        });

        let diagnostics = self.generate_diagnostics(url).await;

        let result = PublishDiagnosticsParams {
            uri: url.clone(),
            diagnostics,
            version: None,
        };

        self.client
            .send_notification::<PublishDiagnostics>(result)
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    },
                )),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Server initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "File saved!")
            .await;

        self.update_document_from_file(&params.text_document.uri)
            .await;
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "File opened!")
            .await;

        self.update_document_from_file(&params.text_document.uri)
            .await;

        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(last) = params.content_changes.last() else {
            return;
        };

        self.client
            .log_message(MessageType::INFO, "File changed!")
            .await;

        self.update_document(&params.text_document.uri, &last.text)
            .await
            .unwrap();
        self.publish_diagnostics(&params.text_document.uri).await;
    }

    async fn did_close(&self, _params: DidCloseTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "File closed!")
            .await;
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
        match params.command.as_str() {
            "AddWord" => {
                let mut string_args = params
                    .arguments
                    .into_iter()
                    .map(|v| serde_json::from_value::<String>(v).unwrap());

                let Some((first, second)) = string_args.next_tuple() else {
                    return Ok(None);
                };

                let mut dict = self.load_user_dictionary().await.unwrap();
                dict.append_word(&first.chars().collect::<Vec<_>>());
                self.save_user_dictionary(dict).await.unwrap();

                self.publish_diagnostics(&Url::parse(&second).unwrap())
                    .await;

                Ok(None)
            }

            _ => Ok(None),
        }
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let actions = self
            .generate_code_actions(&params.text_document.uri, params.range)
            .await?;

        Ok(Some(actions))
    }
}
