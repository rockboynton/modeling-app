//! Inferred types from the execution already used for document diagnostics.
use std::sync::Arc;

use tower_lsp::lsp_types::InlayHint;
use tower_lsp::lsp_types::InlayHintKind;
use tower_lsp::lsp_types::InlayHintLabel;
use tower_lsp::lsp_types::InlayHintParams;
use tower_lsp::lsp_types::TextDocumentItem;

use super::Backend;
use crate::Program;
use crate::execution::ExecutorContext;
use crate::parsing::ast::types::BodyItem;
use crate::parsing::ast::types::Expr;

#[derive(Clone)]
pub(crate) struct HintSnapshot {
    generation: Arc<()>,
    source: String,
    hints: Vec<InlayHint>,
}

pub(super) async fn inferred_type_hints(program: &Program, context: &ExecutorContext) -> Vec<InlayHint> {
    let mut hints = vec![];
    // Runtime memory only exposes root-module bindings, so do not attach their
    // types to similarly named variables in a nested scope.
    for item in &program.ast.body {
        let BodyItem::VariableDeclaration(value) = item else {
            continue;
        };
        let declaration = &value.declaration;
        if matches!(
            declaration.init,
            Expr::FunctionExpression(_) | Expr::AscribedExpression(_)
        ) {
            continue;
        }
        let Some(value) = context.cache.read_old_memory_var(&declaration.id.name).await else {
            continue;
        };
        let Some(ty) = value.principal_type() else {
            continue;
        };
        let Some(position) = kcl_lib::byte_to_lsp_position(&program.original_file_contents, declaration.id.end) else {
            continue;
        };
        hints.push(InlayHint {
            position,
            label: InlayHintLabel::String(format!(": {}", ty.human_friendly_type())),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: Some(false),
            padding_right: Some(false),
            data: None,
        });
    }
    hints
}

impl Backend {
    pub(super) fn begin_hint_snapshot(&self, params: &TextDocumentItem, force: bool) -> Arc<()> {
        let generation = Arc::new(());
        let mut snapshot = self
            .hint_snapshots
            .entry(params.uri.to_string())
            .or_insert_with(|| HintSnapshot {
                generation: generation.clone(),
                source: params.text.clone(),
                hints: vec![],
            });
        if force || snapshot.source != params.text {
            snapshot.hints.clear();
        }
        snapshot.generation = generation.clone();
        snapshot.source.clone_from(&params.text);
        generation
    }

    pub(super) fn finish_hint_snapshot(&self, params: &TextDocumentItem, generation: &Arc<()>, hints: Vec<InlayHint>) {
        let Some(mut snapshot) = self.hint_snapshots.get_mut(params.uri.as_str()) else {
            return;
        };
        if Arc::ptr_eq(&snapshot.generation, generation) {
            snapshot.hints = hints;
        }
    }

    pub(super) fn document_inlay_hints(&self, params: InlayHintParams) -> Option<Vec<InlayHint>> {
        let uri = params.text_document.uri;
        let snapshot = self.hint_snapshots.get(uri.as_str())?;
        // A didChange can update the source before its diagnostic pass starts.
        if self.code_map.get(uri.as_str())?.as_slice() != snapshot.source.as_bytes() {
            return None;
        }
        Some(
            snapshot
                .hints
                .iter()
                .filter(|hint| params.range.start <= hint.position && hint.position < params.range.end)
                .cloned()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::LanguageServer;
    use tower_lsp::lsp_types::*;

    use super::*;

    fn label(hint: &InlayHint) -> &str {
        let InlayHintLabel::String(label) = &hint.label else {
            panic!("expected text label")
        };
        label
    }

    fn params(uri: &Url, range: Range) -> InlayHintParams {
        InlayHintParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range,
            work_done_progress_params: Default::default(),
        }
    }

    fn all() -> Range {
        Range::new(Position::new(0, 0), Position::new(u32::MAX, 0))
    }

    fn item(uri: &Url, source: &str) -> TextDocumentItem {
        TextDocumentItem {
            uri: uri.clone(),
            language_id: "kcl".into(),
            version: 1,
            text: source.into(),
        }
    }

    #[tokio::test]
    async fn inlay_hints_infer_units_and_filter_scope_range_and_annotations() {
        let server = crate::lsp::test_util::kcl_lsp_server_mock_execution().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let uri = Url::from_file_path(dir.path().join("main.kcl")).unwrap();
        let source = "@settings(kclVersion = 3.0)\r\n/* 😀 */\r\nwidth = 20mm\r\nheight = width * 2\r\nexplicit = (2mm: number(mm))\r\nfn double(@width) { local = width * 2\nreturn local }\r\n";
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: item(&uri, source),
            })
            .await;
        let hints = server.inlay_hint(params(&uri, all())).await.unwrap().unwrap();
        assert_eq!(
            hints.len(),
            2,
            "{hints:?}; diagnostics: {:?}",
            server.diagnostics_map.get(uri.as_str()).map(|v| v.value().clone())
        );
        assert_eq!(label(&hints[0]), ": number(mm)");
        assert_eq!(label(&hints[1]), ": number(mm)");
        assert_eq!(
            hints[0].position,
            kcl_lib::byte_to_lsp_position(source, source.find("width =").unwrap() + 5).unwrap()
        );
        assert!(
            hints
                .iter()
                .all(|hint| hint.kind == Some(InlayHintKind::TYPE) && hint.text_edits.is_none())
        );
        let range = Range::new(Position::new(3, 0), Position::new(4, 0));
        let selected = server.inlay_hint(params(&uri, range)).await.unwrap().unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].position, hints[1].position);
        let end = Range::new(Position::new(0, 0), hints[0].position);
        assert!(server.inlay_hint(params(&uri, end)).await.unwrap().unwrap().is_empty());
        // Syntax errors must not keep hints from the last successful run.
        server
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "@settings(kclVersion = 3.0)\nwidth =".into(),
                }],
            })
            .await;
        assert!(
            server
                .inlay_hint(params(&uri, all()))
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        server
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })
            .await;
        assert!(server.inlay_hint(params(&uri, all())).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn inlay_hints_use_unsaved_imports_and_clear_on_execution_errors() {
        let server = crate::lsp::test_util::kcl_lsp_server_mock_execution().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let uri = Url::from_file_path(dir.path().join("main.kcl")).unwrap();
        let dims = Url::from_file_path(dir.path().join("dims.kcl")).unwrap();
        std::fs::write(
            dir.path().join("dims.kcl"),
            "@settings(kclVersion = 3.0)\nexport width = 2mm\n",
        )
        .unwrap();
        let source = "@settings(kclVersion = 3.0)\nimport width from \"dims.kcl\"\nlength = width * 2\n";
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: item(&uri, source),
            })
            .await;
        let hints = server.inlay_hint(params(&uri, all())).await.unwrap().unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(label(&hints[0]), ": number(mm)");
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: item(&dims, "@settings(kclVersion = 3.0)\nexport width = 2deg\n"),
            })
            .await;
        assert_eq!(
            label(&server.inlay_hint(params(&uri, all())).await.unwrap().unwrap()[0]),
            ": number(deg)"
        );
        server
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri: dims, version: 2 },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "@settings(kclVersion = 3.0)\nexport width = \"invalid\"\n".into(),
                }],
            })
            .await;
        assert!(
            server
                .inlay_hint(params(&uri, all()))
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn inlay_hints_reject_outdated_execution_results_and_sources() {
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let uri = Url::parse("file:///hints.kcl").unwrap();
        let first = item(&uri, "@settings(kclVersion = 3.0)\nx = 1mm\n");
        let second = item(&uri, "@settings(kclVersion = 3.0)\nx = 2deg\n");
        let old = server.begin_hint_snapshot(&first, false);
        let current = server.begin_hint_snapshot(&second, false);
        let hint = InlayHint {
            position: Position::new(1, 1),
            label: InlayHintLabel::String(": number(mm)".into()),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        };
        server.code_map.insert(uri.to_string(), second.text.as_bytes().to_vec());
        server.finish_hint_snapshot(&first, &old, vec![hint.clone()]);
        assert!(server.document_inlay_hints(params(&uri, all())).unwrap().is_empty());
        server.finish_hint_snapshot(&second, &current, vec![hint]);
        assert_eq!(server.document_inlay_hints(params(&uri, all())).unwrap().len(), 1);
        server.code_map.insert(uri.to_string(), first.text.into_bytes());
        assert!(server.document_inlay_hints(params(&uri, all())).is_none());
        server.hint_snapshots.remove(uri.as_str());
        server.finish_hint_snapshot(&second, &current, vec![]);
        assert!(!server.hint_snapshots.contains_key(uri.as_str()));
    }

    #[tokio::test]
    async fn inlay_hints_read_shared_execution_cache_without_a_project_context() {
        let context = ExecutorContext::new_mock(None).await;
        let source = "@settings(kclVersion = 3.0)\nvalue = 2mm * 3\n";
        let program = Program::parse_no_errs(source).unwrap();
        context
            .run_mock(&program, &crate::execution::MockConfig::default())
            .await
            .unwrap();
        let hints = inferred_type_hints(&program, &context).await;
        assert_eq!(hints.len(), 1);
        assert_eq!(label(&hints[0]), ": number(mm)");
        context.close().await;
    }
}
