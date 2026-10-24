//! Completion edits for public symbols in the current project.
use std::collections::HashSet;

use tower_lsp::lsp_types::*;

use super::Backend;
use super::navigation::Index;
use super::navigation::Occurrence;
use super::navigation::resolve_occurrence;
use crate::Program;
use crate::SourceRange;
use crate::parsing::ast::types::BodyItem;

fn insertion(source: &str) -> Option<(usize, String)> {
    let program = Program::parse_no_errs(source).ok()?;
    let end = program
        .ast
        .inner_attrs
        .iter()
        .map(|a| a.end)
        .chain(
            program
                .ast
                .body
                .iter()
                .filter_map(|item| matches!(item, BodyItem::ImportStatement(_)).then_some(item.end())),
        )
        .max()
        .unwrap_or(0);
    if end == 0 {
        return Some((0, String::new()));
    }
    Some(match source[end..].find('\n') {
        Some(next) => (end + next + 1, String::new()),
        None => (source.len(), "\n".into()),
    })
}

pub(super) fn items(
    index: &Index,
    uri: &Url,
    prefix: &str,
    range: Range,
    occupied: &HashSet<String>,
) -> Vec<CompletionItem> {
    let Some(document) = index.get(uri) else {
        return vec![];
    };
    let Some((offset, leading)) = insertion(&document.source) else {
        return vec![];
    };
    let Some(position) = kcl_lib::byte_to_lsp_position(&document.source, offset) else {
        return vec![];
    };
    let newline = if document.source.contains("\r\n") { "\r\n" } else { "\n" };
    let leading = if leading.is_empty() { "" } else { newline };
    let mut items = vec![];
    for target in index.keys().filter(|target| *target != uri) {
        let Some(relative) = uri.make_relative(target).filter(|path| !path.contains("://")) else {
            continue;
        };
        let Ok(path) = percent_encoding::percent_decode_str(&relative).decode_utf8() else {
            continue;
        };
        for name in super::completion::exported_names(index, target, &mut HashSet::new()) {
            if !name.starts_with(prefix)
                || occupied.contains(&name)
                || document
                    .bindings
                    .iter()
                    .any(|b| b.import.as_ref() == Some(&(target.clone(), Some(name.clone()))))
            {
                continue;
            }
            let occurrence = Occurrence {
                name: name.clone(),
                qualifier: None,
                span: SourceRange::new(0, 0, Default::default()),
                declaration: false,
                import_name: false,
                callee: None,
                type_context: false,
            };
            let Some(symbol) = resolve_occurrence(index, target, &occurrence, true) else {
                continue;
            };
            let Some(binding) = index.get(&symbol.uri).and_then(|d| {
                d.bindings
                    .iter()
                    .find(|b| b.span.start() == symbol.start && b.span.end() == symbol.end)
            }) else {
                continue;
            };
            let mut item = super::completion::completion(index, &symbol.uri, binding, &name, range);
            let import = format!(
                "{leading}import {name} from {}{newline}",
                serde_json::to_string(path.as_ref()).unwrap_or_default()
            );
            item.detail = Some(format!("Import from {path}"));
            item.sort_text = Some(format!("1{name}:{path}"));
            if range.start == position {
                item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: format!("{import}{name}"),
                }));
            } else {
                item.additional_text_edits = Some(vec![TextEdit {
                    range: Range::new(position, position),
                    new_text: import,
                }]);
            }
            items.push(item);
        }
    }
    items
}

impl Backend {
    pub(super) async fn auto_import_completions(
        &self,
        uri: &Url,
        source: &str,
        offset: usize,
        range: Range,
        occupied: HashSet<String>,
    ) -> Vec<CompletionItem> {
        let Some(start) = kcl_lib::lsp_position_to_byte(source, range.start) else {
            return vec![];
        };
        let Some(prefix) = source.get(start..offset).filter(|prefix| prefix.chars().count() >= 2) else {
            return vec![];
        };
        let index = self.navigation_index(uri, true).await;
        #[cfg(not(target_arch = "wasm32"))]
        let index = {
            let mut index = index;
            if let Ok(path) = uri.to_file_path() {
                let root = self.document_project_root(&path);
                index.retain(|target, _| target.to_file_path().is_ok_and(|path| path.starts_with(&root)));
            }
            index
        };
        items(&index, uri, prefix, range, &occupied)
    }
}

#[cfg(test)]
mod tests {
    use super::super::navigation::Document;
    use super::*;

    #[test]
    fn auto_import_edits_preserve_headers_comments_unicode_and_export_visibility() {
        let uri = Url::parse("file:///project/main.kcl").unwrap();
        let target = Url::parse("file:///project/cabinet%20dims.kcl").unwrap();
        let source = "@settings(kclVersion = 3.0)\r\n/// Keep these docs.\r\nfn f() {\r\n/* 😀 */\r\nreturn wid }\r\n";
        let imported = "@settings(kclVersion = 3.0)\nexport width = 20mm\nwidthPrivate = 1mm\n";
        let mut index = Index::new();
        index.insert(uri.clone(), Document::parse(&uri, source.into()).unwrap());
        index.insert(target.clone(), Document::parse(&target, imported.into()).unwrap());
        let start = source.find("wid }").unwrap();
        let range = Range::new(
            kcl_lib::byte_to_lsp_position(source, start).unwrap(),
            kcl_lib::byte_to_lsp_position(source, start + 3).unwrap(),
        );
        let result = items(&index, &uri, "wid", range, &HashSet::new());
        assert_eq!(result.len(), 1);
        let edit = &result[0].additional_text_edits.as_ref().unwrap()[0];
        let offset = kcl_lib::lsp_position_to_byte(source, edit.range.start).unwrap();
        let mut applied = source.replacen("wid }", "width }", 1);
        applied.insert_str(offset, &edit.new_text);
        assert!(applied.contains("import width from \"cabinet dims.kcl\"\r\n/// Keep these docs."));
        Program::parse_no_errs(&applied).unwrap();
        assert!(items(&index, &uri, "wid", range, &HashSet::from(["width".into()])).is_empty());
    }
    #[tokio::test]
    async fn auto_import_discovers_unopened_project_exports() {
        use tower_lsp::LanguageServer;
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("dims.kcl"),
            "@settings(kclVersion = 3.0)\nexport width = 20mm\n",
        )
        .unwrap();
        let uri = Url::from_file_path(dir.path().join("main.kcl")).unwrap();
        let source = "@settings(kclVersion = 3.0)\nresult = wid\n";
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "kcl".into(),
                    version: 1,
                    text: source.into(),
                },
            })
            .await;
        let start = source.find("wid").unwrap();
        let range = Range::new(
            kcl_lib::byte_to_lsp_position(source, start).unwrap(),
            kcl_lib::byte_to_lsp_position(source, start + 3).unwrap(),
        );
        let result = server
            .auto_import_completions(&uri, source, start + 3, range, HashSet::new())
            .await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].label, "width");
        assert!(
            result[0].additional_text_edits.as_ref().unwrap()[0]
                .new_text
                .contains("dims.kcl")
        );
    }
}
