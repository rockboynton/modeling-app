//! Documentation for source bindings, independent of execution memory.
use tower_lsp::lsp_types::Hover;
use tower_lsp::lsp_types::HoverContents;
use tower_lsp::lsp_types::MarkupContent;
use tower_lsp::lsp_types::MarkupKind;
use tower_lsp::lsp_types::Range;
use tower_lsp::lsp_types::TextDocumentPositionParams;

use super::Backend;
use super::navigation::Index;
use super::navigation::Occurrence;
use super::navigation::Symbol;
use super::navigation::resolve_occurrence;
use crate::Program;
use crate::SourceRange;
use crate::parsing::ast::types::Expr;
use crate::parsing::token::TokenType;
use crate::parsing::token::adapter;
use crate::walk::Node;

pub(super) fn documentation(comments: &[String]) -> String {
    comments
        .iter()
        .map(|comment| {
            if let Some(block) = comment.strip_prefix("/*").and_then(|text| text.strip_suffix("*/")) {
                block
                    .lines()
                    .map(|line| {
                        let line = line.trim_start();
                        {
                            let line = line.strip_prefix('*').unwrap_or(line);
                            line.strip_prefix(' ').unwrap_or(line).to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                comment
                    .lines()
                    .map(|line| {
                        let line = line.trim_start();
                        let line = line
                            .strip_prefix("///")
                            .or_else(|| line.strip_prefix("//!"))
                            .or_else(|| line.strip_prefix("//"))
                            .unwrap_or(line);
                        line.strip_prefix(' ').unwrap_or(line).to_owned()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

pub(super) struct SourceDocumentation {
    pub hover: Hover,
    pub docs: String,
    pub prefer_source: bool,
}

fn hover(label: String, docs: String, range: Range, prefer_source: bool) -> SourceDocumentation {
    let hover = Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: if docs.is_empty() {
                format!("```kcl\n{label}\n```")
            } else {
                format!("```kcl\n{label}\n```\n\n{docs}")
            },
        }),
        range: Some(range),
    };
    SourceDocumentation {
        hover,
        docs,
        prefer_source,
    }
}

fn declaration_hover(index: &Index, symbol: &Symbol, name: &str, range: Range) -> Option<SourceDocumentation> {
    let document = index.get(&symbol.uri)?;
    let program = Program::parse_no_errs(&document.source).ok()?;
    let selected = |span: SourceRange| span.start() == symbol.start && span.end() == symbol.end;
    let result = std::cell::RefCell::new(None);
    crate::walk::walk(&program.ast, |node| {
        match node {
            Node::VariableDeclaration(value) if selected(SourceRange::from(&value.declaration.id)) => {
                let declaration = &value.declaration;
                let label = match &declaration.init {
                    Expr::FunctionExpression(function) => format!("fn {name}{}", function.signature()),
                    Expr::AscribedExpression(value) => format!("{name}: {}", value.ty),
                    expression => document
                        .source
                        .get(expression.start()..expression.end())
                        .filter(|text| !text.contains(['\r', '\n']) && text.len() <= 160)
                        .map(|text| format!("{name} = {text}"))
                        .unwrap_or_else(|| name.into()),
                };
                result.replace(Some(hover(
                    label,
                    documentation(&value.pre_comments),
                    range,
                    matches!(declaration.init, Expr::FunctionExpression(_)),
                )));
                return Ok::<_, anyhow::Error>(false);
            }
            Node::TypeDeclaration(value) if selected(SourceRange::from(&value.name)) => {
                result.replace(Some(hover(
                    format!("type {name}"),
                    documentation(&value.pre_comments),
                    range,
                    true,
                )));
                return Ok(false);
            }
            Node::Parameter(value) if selected(SourceRange::from(&value.identifier)) => {
                let label = value
                    .param_type
                    .as_ref()
                    .map(|ty| format!("{name}: {ty}"))
                    .unwrap_or_else(|| name.into());
                result.replace(Some(hover(
                    label,
                    documentation(&value.identifier.pre_comments),
                    range,
                    false,
                )));
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    })
    .ok()?;
    result.into_inner()
}

fn module_hover(
    index: &Index,
    target: &tower_lsp::lsp_types::Url,
    label: &str,
    range: Range,
) -> Option<SourceDocumentation> {
    let document = index.get(target)?;
    let tokens = adapter::lex_with_diagnostics(&document.source, Default::default()).tokens;
    let comments: Vec<_> = tokens
        .iter()
        .take_while(|token| !token.is_code_token())
        .filter(|token| token.token_type.is_comment())
        .filter_map(|token| document.source.get(token.start..token.end).map(str::to_owned))
        .collect();
    Some(hover(format!("module {label}"), documentation(&comments), range, true))
}

impl Backend {
    pub(super) async fn source_hover(&self, params: &TextDocumentPositionParams) -> Option<SourceDocumentation> {
        let uri = &params.text_document.uri;
        let index = self.navigation_index(uri, false).await;
        let document = index.get(uri)?;
        let offset = kcl_lib::lsp_position_to_byte(&document.source, params.position)?;
        let range = |span: SourceRange| {
            Some(Range::new(
                kcl_lib::byte_to_lsp_position(&document.source, span.start())?,
                kcl_lib::byte_to_lsp_position(&document.source, span.end())?,
            ))
        };
        let occurrence: Option<&Occurrence> = document
            .occurrences
            .iter()
            .filter(|item| item.span.start() <= offset && offset < item.span.end())
            .min_by_key(|item| item.span.end() - item.span.start());
        if let Some(occurrence) = occurrence {
            let symbol = resolve_occurrence(&index, uri, occurrence, true)?;
            let selected_range = range(occurrence.span)?;
            if let Some(binding) = index
                .get(&symbol.uri)?
                .bindings
                .iter()
                .find(|binding| binding.span.start() == symbol.start && binding.span.end() == symbol.end)
                && let Some((target, None)) = &binding.import
            {
                return module_hover(&index, target, &occurrence.name, selected_range);
            }
            let name = document.source.get(occurrence.span.start()..occurrence.span.end())?;
            return declaration_hover(&index, &symbol, name, selected_range);
        }
        let tokens = adapter::lex_with_diagnostics(&document.source, Default::default()).tokens;
        let token = tokens
            .iter()
            .find(|token| token.token_type == TokenType::String && token.start <= offset && offset < token.end)?;
        let (_, target) = document
            .import_links
            .iter()
            .find(|(span, _)| span.start() <= token.start && token.end <= span.end())?;
        module_hover(
            &index,
            target,
            target.path().rsplit('/').next()?,
            range(SourceRange::from(token))?,
        )
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::LanguageServer;
    use tower_lsp::lsp_types::*;

    use super::*;

    const HEADER: &str = "@settings(kclVersion = 3.0)\n";

    async fn open(server: &Backend, uri: &Url, source: &str) {
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
    }

    async fn at(server: &Backend, uri: &Url, source: &str, needle: &str) -> Hover {
        let position = kcl_lib::byte_to_lsp_position(source, source.find(needle).unwrap() + 1).unwrap();
        server
            .hover(HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap()
            .unwrap()
    }

    fn markdown(hover: &Hover) -> &str {
        let HoverContents::Markup(content) = &hover.contents else {
            panic!("expected Markdown")
        };
        assert_eq!(content.kind, MarkupKind::Markdown);
        &content.value
    }

    #[tokio::test]
    async fn source_hover_documents_functions_variables_and_unicode_references_without_execution() {
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let uri = Url::parse("file:///user-hover.kcl").unwrap();
        let source = format!("{HEADER}/// Compute twice the distance.\n///\n/// Keeps **millimetres**.\nfn twice(@distance: number(mm)): number(mm) {{ return distance * 2 }}\n/// Cabinet width.\nwidth = 20mm\nresult = twice(width)\nvalues = [\"😀\", width]\n").replace('\n', "\r\n");
        open(&server, &uri, &source).await;
        let function = at(&server, &uri, &source, "twice(width)").await;
        assert!(
            markdown(&function).contains("fn twice(@distance: number(mm)): number(mm)"),
            "{}",
            markdown(&function)
        );
        assert!(
            markdown(&function).contains("Compute twice the distance.\n\nKeeps **millimetres**."),
            "{}",
            markdown(&function)
        );
        let definition = at(&server, &uri, &source, "twice(@").await;
        assert_eq!(markdown(&definition), markdown(&function));
        let variable = at(&server, &uri, &source, "width]").await;
        assert!(markdown(&variable).contains("width = 20mm"));
        assert!(markdown(&variable).contains("Cabinet width."));
        let offset = source.find("width]").unwrap();
        assert_eq!(
            variable.range.unwrap(),
            Range::new(
                kcl_lib::byte_to_lsp_position(&source, offset).unwrap(),
                kcl_lib::byte_to_lsp_position(&source, offset + 5).unwrap()
            )
        );
        let parameter = at(&server, &uri, &source, "distance *").await;
        assert!(markdown(&parameter).contains("distance: number(mm)"));
    }

    #[tokio::test]
    async fn source_hover_resolves_import_aliases_reexports_modules_and_open_buffers() {
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let main = Url::from_file_path(dir.path().join("main.kcl")).unwrap();
        let dims = Url::from_file_path(dir.path().join("dims.kcl")).unwrap();
        let disk = format!(
            "//! Dimensions module.\n{HEADER}/// Double the distance.\nexport fn twice(@distance: number(mm)): number(mm) {{ return distance * 2 }}\n"
        );
        std::fs::write(dir.path().join("dims.kcl"), &disk).unwrap();
        std::fs::write(
            dir.path().join("public.kcl"),
            format!("{HEADER}export import twice as size from \"dims.kcl\"\n"),
        )
        .unwrap();
        let source = format!(
            "{HEADER}import size as measure from \"public.kcl\"\nimport \"dims.kcl\" as dims\nimport * from \"dims.kcl\"\na = measure(2mm)\nb = dims::twice(3mm)\nc = twice(4mm)\n"
        );
        open(&server, &main, &source).await;
        for (needle, name) in [
            ("measure(2", "measure"),
            ("twice(3", "twice"),
            ("twice(4", "twice"),
            ("size as", "size"),
        ] {
            let result = at(&server, &main, &source, needle).await;
            assert!(
                markdown(&result).contains(&format!("fn {name}(@distance: number(mm))")),
                "{}",
                markdown(&result)
            );
            assert!(markdown(&result).contains("Double the distance."));
        }
        let module = at(&server, &main, &source, "dims::").await;
        assert!(markdown(&module).contains("module dims"));
        assert!(markdown(&module).contains("Dimensions module."));
        let path = at(&server, &main, &source, "\"dims.kcl\"").await;
        assert!(markdown(&path).contains("Dimensions module."));
        let unsaved = disk
            .replace("Double the distance.", "Unsaved function documentation.")
            .replace("number(mm)", "number(cm)");
        open(&server, &dims, &unsaved).await;
        let result = at(&server, &main, &source, "measure(2").await;
        assert!(markdown(&result).contains("Unsaved function documentation."));
        assert!(markdown(&result).contains("number(cm)"));
        server
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: dims },
            })
            .await;
        let result = at(&server, &main, &source, "measure(2").await;
        assert!(markdown(&result).contains("Double the distance."));
        assert!(markdown(&result).contains("number(mm)"));
        assert_eq!(std::fs::read_to_string(dir.path().join("dims.kcl")).unwrap(), disk);
    }

    #[tokio::test]
    async fn source_hover_preserves_runtime_values() {
        let server = crate::lsp::test_util::kcl_lsp_server_mock_execution().await.unwrap();
        let uri = Url::parse("untitled:runtime-hover.kcl").unwrap();
        let source = format!("{HEADER}/// Computed width.\nwidth = 2mm + 3mm\ncopy = width\n");
        open(&server, &uri, &source).await;
        let result = at(&server, &uri, &source, "width\n").await;
        assert!(markdown(&result).contains("width: number(mm)"), "{}", markdown(&result));
        assert!(markdown(&result).contains("5"), "{}", markdown(&result));
        assert!(markdown(&result).contains("Computed width."));
    }

    #[tokio::test]
    async fn source_hover_respects_shadowing_and_preserves_stdlib_documentation() {
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let uri = Url::parse("file:///shadow-hover.kcl").unwrap();
        let source = format!(
            "{HEADER}/// Outer helper.\nfn helper(@outside) {{ return outside }}\nfn container() {{\n/* Inner helper.\n * Has a separate scope.\n */\nfn helper(@inside) {{ return inside }}\nreturn helper(2)\n}}\nresult = helper(3)\nvalue = cos(0deg)\n"
        );
        open(&server, &uri, &source).await;
        let inside = at(&server, &uri, &source, "helper(2").await;
        assert!(markdown(&inside).contains("@inside"));
        assert!(
            markdown(&inside).contains("Inner helper.\nHas a separate scope."),
            "{}",
            markdown(&inside)
        );
        assert!(!markdown(&inside).contains("Outer helper."));
        let outside = at(&server, &uri, &source, "helper(3").await;
        assert!(markdown(&outside).contains("@outside"));
        assert!(markdown(&outside).contains("Outer helper."));
        let builtin = at(&server, &uri, &source, "cos(0").await;
        assert!(markdown(&builtin).contains("cos("));
        assert!(markdown(&builtin).len() > 50);
    }
}
