//! Scope-aware completion using the same binding identities as navigation.
use std::collections::BTreeSet;
use std::collections::HashSet;

use tower_lsp::lsp_types::CompletionItem;
use tower_lsp::lsp_types::CompletionItemKind;
use tower_lsp::lsp_types::CompletionTextEdit;
use tower_lsp::lsp_types::InsertTextFormat;
use tower_lsp::lsp_types::Range;
use tower_lsp::lsp_types::TextEdit;
use tower_lsp::lsp_types::Url;

use super::Backend;
use super::navigation::Binding;
use super::navigation::Document;
use super::navigation::Index;
use super::navigation::Occurrence;
use super::navigation::namespace_target;
use super::navigation::resolve_occurrence;
use crate::ModuleId;
use crate::SourceRange;
use crate::parsing::token::TokenType;
use crate::parsing::token::adapter;

pub(super) struct LexicalCompletions {
    pub items: Vec<CompletionItem>,
    pub namespace: Option<String>,
    pub range: Range,
    pub excluded: bool,
}

fn context(source: &str, offset: usize) -> Option<(Range, Option<String>, bool)> {
    let tokens = adapter::lex_with_diagnostics(source, ModuleId::default()).tokens;
    let tokens = tokens.as_slice();
    let excluded = tokens.iter().any(|token| {
        token.start <= offset
            && offset <= token.end
            && (token.token_type.is_comment()
                || (token.token_type == TokenType::String && token.start < offset && offset < token.end))
    });
    let word = tokens.iter().find(|token| {
        token.start <= offset
            && offset <= token.end
            && matches!(token.token_type, TokenType::Word | TokenType::Type | TokenType::Keyword)
    });
    let (start, end) = word.map(|token| (token.start, token.end)).unwrap_or((offset, offset));
    let range = Range::new(
        kcl_lib::byte_to_lsp_position(source, start)?,
        kcl_lib::byte_to_lsp_position(source, end)?,
    );
    let previous: Vec<_> = tokens
        .iter()
        .filter(|token| token.end <= start && token.is_code_token())
        .collect();
    let mut at = previous.len();
    let mut names = vec![];
    while at >= 2
        && matches!(
            source.get(previous[at - 1].start..previous[at - 1].end),
            Some("." | "::")
        )
    {
        let word = previous[at - 2];
        if !matches!(word.token_type, TokenType::Word | TokenType::Type) {
            break;
        }
        names.push(source.get(word.start..word.end)?.to_owned());
        at -= 2;
    }
    names.reverse();
    Some((range, (!names.is_empty()).then(|| names.join("::")), excluded))
}

fn occurrence(name: &str, span: SourceRange, qualifier: Option<String>) -> Occurrence {
    Occurrence {
        name: name.into(),
        qualifier,
        span,
        declaration: false,
        import_name: false,
        callee: None,
        type_context: false,
    }
}

fn completion(index: &Index, uri: &Url, binding: &Binding, label: &str, range: Range) -> CompletionItem {
    let resolved = resolve_occurrence(index, uri, &occurrence(&binding.name, binding.span, None), true);
    let (document, source) = resolved
        .as_ref()
        .and_then(|symbol| {
            let document = index.get(&symbol.uri)?;
            let binding = document
                .bindings
                .iter()
                .find(|binding| binding.span.start() == symbol.start && binding.span.end() == symbol.end)?;
            Some((document, binding))
        })
        .unwrap_or((&index[uri], binding));
    let kind = if source.import.as_ref().is_some_and(|(_, name)| name.is_none()) {
        CompletionItemKind::MODULE
    } else if document.functions.iter().any(|(span, _)| *span == source.span) {
        CompletionItemKind::FUNCTION
    } else if document.types.contains(&source.span) {
        CompletionItemKind::CLASS
    } else if document.functions.iter().any(|(_, scope)| *scope == source.scope) {
        CompletionItemKind::VARIABLE
    } else if document
        .source
        .get(source.span.start()..source.span.end())
        .is_some_and(|text| text.starts_with('$'))
    {
        CompletionItemKind::REFERENCE
    } else {
        CompletionItemKind::CONSTANT
    };
    CompletionItem {
        label: label.into(),
        kind: Some(kind),
        detail: binding.import.as_ref().map(|(uri, _)| format!("from {}", uri.path())),
        insert_text: Some(label.into()),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: label.into(),
        })),
        ..Default::default()
    }
}

fn exported_names(index: &Index, uri: &Url, visited: &mut HashSet<Url>) -> BTreeSet<String> {
    if !visited.insert(uri.clone()) {
        return BTreeSet::new();
    }
    let Some(document) = index.get(uri) else {
        return BTreeSet::new();
    };
    let mut names: BTreeSet<_> = document
        .bindings
        .iter()
        .filter(|binding| binding.exported)
        .map(|binding| binding.name.clone())
        .collect();
    for (target, _, exported) in &document.globs {
        if *exported {
            names.extend(exported_names(index, target, visited));
        }
    }
    names
}

fn binding_items(
    index: &Index,
    uri: &Url,
    offset: usize,
    range: Range,
    namespace: Option<&str>,
) -> Vec<CompletionItem> {
    let Some(document) = index.get(uri) else {
        return vec![];
    };
    if let Some(namespace) = namespace {
        let Some(target) = namespace_target(index, uri, namespace, offset) else {
            return vec![];
        };
        return exported_names(index, target, &mut HashSet::new())
            .into_iter()
            .filter_map(|name| {
                let symbol = resolve_occurrence(
                    index,
                    uri,
                    &occurrence(
                        &name,
                        SourceRange::new(offset, offset, Default::default()),
                        Some(namespace.into()),
                    ),
                    true,
                )?;
                let binding = index
                    .get(&symbol.uri)?
                    .bindings
                    .iter()
                    .find(|binding| binding.span.start() == symbol.start && binding.span.end() == symbol.end)?;
                Some(completion(index, &symbol.uri, binding, &name, range))
            })
            .collect();
    }
    let mut items = vec![];
    let mut seen = HashSet::new();
    for binding in &document.bindings {
        if document
            .binding(&binding.name, offset)
            .is_some_and(|visible| visible.span == binding.span)
            && seen.insert(binding.name.clone())
        {
            items.push(completion(index, uri, binding, &binding.name, range));
        }
    }
    for (target, scope, _) in &document.globs {
        if scope.start() > offset || offset > scope.end() {
            continue;
        }
        for name in exported_names(index, target, &mut HashSet::new()) {
            if seen.contains(&name) {
                continue;
            }
            let Some(symbol) = resolve_occurrence(
                index,
                uri,
                &occurrence(&name, SourceRange::new(offset, offset, Default::default()), None),
                true,
            ) else {
                continue;
            };
            if let Some(binding) = index.get(&symbol.uri).and_then(|document| {
                document
                    .bindings
                    .iter()
                    .find(|binding| binding.span.start() == symbol.start && binding.span.end() == symbol.end)
            }) {
                items.push(completion(index, &symbol.uri, binding, &name, range));
                seen.insert(name);
            }
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

impl Backend {
    pub(super) async fn lexical_completions(
        &self,
        uri: &Url,
        source: &str,
        offset: usize,
    ) -> Option<LexicalCompletions> {
        let (range, namespace, excluded) = context(source, offset)?;
        if excluded {
            return Some(LexicalCompletions {
                items: vec![],
                range,
                namespace,
                excluded,
            });
        }
        // Inserting a temporary identifier recovers common editing states such
        // as `dims.` and `value = ` without changing the editor's buffer.
        let recovered = if Document::parse(uri, source.into()).is_none() {
            let mut source = source.to_owned();
            source.insert_str(offset, "completionPlaceholder");
            source
        } else {
            source.to_owned()
        };
        let (index, _) = self.navigation_index_with_source(uri, false, Some(recovered)).await;
        if let Some(fields) = super::shapes::member_fields(&index, uri, offset) {
            let items = fields
                .into_iter()
                .map(|(name, shape)| CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: (!shape.label.is_empty()).then_some(shape.label),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: name })),
                    ..Default::default()
                })
                .collect();
            return Some(LexicalCompletions {
                items,
                namespace: Some(String::new()),
                range,
                excluded,
            });
        }
        Some(LexicalCompletions {
            items: binding_items(&index, uri, offset, range, namespace.as_deref()),
            namespace,
            range,
            excluded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "@settings(kclVersion = 3.0)\n";

    fn document(index: &mut Index, name: &str, source: &str) -> Url {
        let uri = Url::parse(&format!("file:///completion/{name}")).unwrap();
        index.insert(uri.clone(), Document::parse(&uri, format!("{HEADER}{source}")).unwrap());
        uri
    }

    fn items(index: &Index, uri: &Url, marker: &str, namespace: Option<&str>) -> Vec<CompletionItem> {
        let offset = index[uri].source.find(marker).unwrap();
        binding_items(index, uri, offset, Range::default(), namespace)
    }

    #[test]
    fn scoped_completions_keep_only_visible_bindings() {
        let mut index = Index::new();
        let uri = document(
            &mut index,
            "main.kcl",
            "outer = 1\nfn first(@param) {\nlocal = param\nreturn local\n}\nfn second(@other) { return other }\nresult = outer\n",
        );
        let inside = items(&index, &uri, "return local", None);
        let names: Vec<_> = inside.iter().map(|item| item.label.as_str()).collect();
        assert!(names.contains(&"param") && names.contains(&"local") && names.contains(&"outer"));
        assert!(!names.contains(&"other"));
        assert_eq!(
            inside.iter().find(|item| item.label == "first").unwrap().kind,
            Some(CompletionItemKind::FUNCTION)
        );
        let outside = items(&index, &uri, "result =", None);
        assert!(
            !outside
                .iter()
                .any(|item| ["param", "local", "other"].contains(&item.label.as_str()))
        );
    }

    #[test]
    fn scoped_completions_follow_module_exports_aliases_and_globs() {
        let mut index = Index::new();
        document(
            &mut index,
            "dims.kcl",
            "export width = 20mm\nprivate = 1\nexport fn measure(@value) { return value }\nexport type Length = number(mm)\n",
        );
        document(&mut index, "public.kcl", "export import * from \"dims.kcl\"\n");
        let uri = document(
            &mut index,
            "main.kcl",
            "import \"public.kcl\" as dims\nimport measure as size from \"dims.kcl\"\nvalue = dims.width\n",
        );
        let members = items(&index, &uri, "dims.width", Some("dims"));
        assert_eq!(
            members.iter().map(|item| item.label.as_str()).collect::<Vec<_>>(),
            ["Length", "measure", "width"]
        );
        assert_eq!(members[0].kind, Some(CompletionItemKind::CLASS));
        assert_eq!(members[1].kind, Some(CompletionItemKind::FUNCTION));
        let globals = items(&index, &uri, "value =", None);
        assert_eq!(
            globals.iter().find(|item| item.label == "size").unwrap().kind,
            Some(CompletionItemKind::FUNCTION)
        );
        assert_eq!(
            globals.iter().find(|item| item.label == "dims").unwrap().kind,
            Some(CompletionItemKind::MODULE)
        );
        let ambiguous = document(
            &mut index,
            "ambiguous.kcl",
            "import * from \"dims.kcl\"\nimport * from \"other.kcl\"\nvalue = 1\n",
        );
        document(&mut index, "other.kcl", "export width = 10mm\n");
        assert!(
            !items(&index, &ambiguous, "value =", None)
                .iter()
                .any(|item| item.label == "width")
        );
    }

    #[test]
    fn scoped_completion_context_replaces_full_word_in_utf16() {
        let source = "values = [\"😀\", dims.width]\n";
        let cursor = source.find("width").unwrap() + 2;
        let (range, namespace, excluded) = context(source, cursor).unwrap();
        assert_eq!(namespace.as_deref(), Some("dims"));
        assert!(!excluded);
        assert_eq!(kcl_lib::lsp_position_to_byte(source, range.start), source.find("width"));
        assert_eq!(kcl_lib::lsp_position_to_byte(source, range.end), Some(cursor + 3));
        assert!(context(source, source.find('😀').unwrap()).unwrap().2);
        assert!(context("// comment", 5).unwrap().2);
    }

    #[tokio::test]
    async fn scoped_completions_shadow_builtins_without_duplicates() {
        use tower_lsp::LanguageServer;
        use tower_lsp::lsp_types::*;
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let uri = Url::parse("file:///completion.kcl").unwrap();
        let source = format!("{HEADER}extrude = 1\nvalue = extrude\n");
        server.code_map.insert(uri.to_string(), source.as_bytes().to_vec());
        let offset = source.rfind("extrude").unwrap() + 3;
        let result = server
            .completion(CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: kcl_lib::byte_to_lsp_position(&source, offset).unwrap(),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            })
            .await
            .unwrap()
            .unwrap();
        let CompletionResponse::Array(items) = result else {
            panic!("expected array")
        };
        let matching: Vec<_> = items.iter().filter(|item| item.label == "extrude").collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].kind, Some(CompletionItemKind::CONSTANT));
        assert!(matching[0].text_edit.is_some());
    }

    #[tokio::test]
    async fn scoped_completions_recover_incomplete_members_and_use_open_imports() {
        let dir = tempfile::tempdir().unwrap();
        let main = Url::from_file_path(dir.path().join("main.kcl")).unwrap();
        let dims = Url::from_file_path(dir.path().join("dims.kcl")).unwrap();
        std::fs::write(dir.path().join("dims.kcl"), format!("{HEADER}export diskOnly = 1\n")).unwrap();
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        server
            .code_map
            .insert(dims.to_string(), format!("{HEADER}export unsaved = 2\n").into_bytes());
        server.open_documents.insert(dims.to_string(), 2);
        for member in ["dims.", "dims::"] {
            let source = format!("{HEADER}import \"dims.kcl\" as dims\nvalue = {member}");
            server.code_map.insert(main.to_string(), source.as_bytes().to_vec());
            let result = server.lexical_completions(&main, &source, source.len()).await.unwrap();
            assert_eq!(result.namespace.as_deref(), Some("dims"));
            assert_eq!(
                result.items.iter().map(|item| item.label.as_str()).collect::<Vec<_>>(),
                ["unsaved"],
                "member: {member}"
            );
            assert_eq!(
                server.code_map.get(main.as_str()).unwrap().as_slice(),
                source.as_bytes()
            );
        }
    }
}
