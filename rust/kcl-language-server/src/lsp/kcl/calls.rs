//! Source-resolved callable signatures and cursor context.
use std::cell::RefCell;

use tower_lsp::lsp_types::*;

use super::Backend;
use super::navigation::Index;
use super::navigation::Occurrence;
use super::navigation::resolve_occurrence;
use crate::Program;
use crate::SourceRange;
use crate::parsing::ast::types::Expr;
use crate::parsing::ast::types::FunctionExpression;
use crate::parsing::token::adapter;
use crate::walk::Node;

pub(super) struct SourceFunction {
    pub ast: FunctionExpression,
    pub docs: String,
}

pub(super) fn function(index: &Index, uri: &Url, occurrence: &Occurrence) -> Option<SourceFunction> {
    let symbol = resolve_occurrence(index, uri, occurrence, true)?;
    let document = index.get(&symbol.uri)?;
    let program = Program::parse_no_errs(&document.source).ok()?;
    let result = RefCell::new(None);
    crate::walk::walk(&program.ast, |node| {
        if let Node::VariableDeclaration(value) = node
            && value.declaration.id.start == symbol.start
            && value.declaration.id.end == symbol.end
            && let Expr::FunctionExpression(function) = &value.declaration.init
        {
            result.replace(Some(SourceFunction {
                ast: (***function).clone(),
                docs: super::source_hover::documentation(&value.pre_comments),
            }));
            return Ok::<_, anyhow::Error>(false);
        }
        Ok(true)
    })
    .ok()?;
    result.into_inner()
}

pub(super) struct CallContext {
    pub callee: Occurrence,
    pub argument: usize,
    pub label: Option<String>,
}

pub(super) fn call_context(source: &str, offset: usize) -> Option<CallContext> {
    let tokens = adapter::lex_with_diagnostics(source, Default::default()).tokens;
    if tokens.iter().any(|t| {
        t.start < offset
            && offset < t.end
            && (t.token_type.is_comment() || t.token_type == crate::parsing::token::TokenType::String)
    }) {
        return None;
    }
    let tokens: Vec<_> = tokens
        .iter()
        .filter(|t| t.is_code_token() && t.start < offset)
        .collect();
    let text = |i: usize| &source[tokens[i].start..tokens[i].end];
    let mut stack = vec![];
    for i in 0..tokens.len() {
        match text(i) {
            "(" | "[" | "{" => stack.push(i),
            ")" | "]" | "}" => {
                stack.pop();
            }
            _ => {}
        }
    }
    let open = *stack.iter().rev().find(|&&i| {
        text(i) == "("
            && i > 0
            && matches!(
                tokens[i - 1].token_type,
                crate::parsing::token::TokenType::Word | crate::parsing::token::TokenType::Type
            )
    })?;
    let last = open - 1;
    let mut start = last;
    while start >= 2 && text(start - 1) == "::" {
        start -= 2;
    }
    let qualifier = (start != last).then(|| source[tokens[start].start..tokens[last - 1].start].to_owned());
    let mut depth = 0;
    let mut argument = 0;
    let mut arg_start = open + 1;
    for (i, _) in tokens.iter().enumerate().skip(open + 1) {
        match text(i) {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth -= 1,
            "," if depth == 0 => {
                argument += 1;
                arg_start = i + 1;
            }
            _ => {}
        }
    }
    let label = (arg_start + 1 < tokens.len() && text(arg_start + 1) == "=").then(|| text(arg_start).to_owned());
    Some(CallContext {
        callee: Occurrence {
            name: text(last).into(),
            qualifier,
            span: SourceRange::new(tokens[last].start, tokens[last].end, Default::default()),
            declaration: false,
            import_name: false,
            callee: None,
            type_context: false,
        },
        argument,
        label,
    })
}

impl Backend {
    pub(super) async fn source_signature_help(&self, params: &TextDocumentPositionParams) -> Option<SignatureHelp> {
        let uri = &params.text_document.uri;
        let source = String::from_utf8(self.code_map.get(uri.as_str())?.clone()).ok()?;
        let offset = kcl_lib::lsp_position_to_byte(&source, params.position)?;
        let call = call_context(&source, offset)?;
        let index = self.navigation_index(uri, false).await;
        let function = function(&index, uri, &call.callee)?;
        let active = call
            .label
            .as_ref()
            .and_then(|label| function.ast.params.iter().position(|p| p.identifier.name == *label))
            .unwrap_or(call.argument);
        let parameters = function
            .ast
            .params
            .iter()
            .map(|p| ParameterInformation {
                label: ParameterLabel::Simple(format!(
                    "{}{}{}{}",
                    if p.labeled { "" } else { "@" },
                    p.identifier.name,
                    if p.optional() { "?" } else { "" },
                    p.param_type.as_ref().map(|t| format!(": {t}")).unwrap_or_default()
                )),
                documentation: None,
            })
            .collect::<Vec<_>>();
        let active = (active < parameters.len()).then_some(active as u32);
        Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label: format!("{}{}", call.callee.name, function.ast.signature()),
                documentation: (!function.docs.is_empty()).then_some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: function.docs,
                })),
                parameters: Some(parameters),
                active_parameter: active,
            }],
            active_signature: Some(0),
            active_parameter: active,
        })
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::LanguageServer;

    use super::*;

    #[test]
    fn call_context_tracks_nested_arguments_and_ignores_strings() {
        let source = "answer = dims::size([1, 2], depth = other(3, 4), width = 5)";
        let call = call_context(source, source.find("5)").unwrap()).unwrap();
        assert_eq!(call.callee.name, "size");
        assert_eq!(call.callee.qualifier.as_deref(), Some("dims"));
        assert_eq!(call.argument, 2);
        assert_eq!(call.label.as_deref(), Some("width"));
        let inner = call_context(source, source.find("4)").unwrap()).unwrap();
        assert_eq!(inner.callee.name, "other");
        assert_eq!(inner.argument, 1);
        assert!(call_context("x = size(\"abc\")", 12).is_none());
    }

    #[tokio::test]
    async fn source_signatures_follow_imports_and_named_argument_order() {
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dims.kcl"), "@settings(kclVersion = 3.0)\n/// Cabinet dimensions.\nexport fn size(@width: number(mm), depth: number(mm), height?: number(mm) = 2mm): number(mm) { return width }\n").unwrap();
        let uri = Url::from_file_path(dir.path().join("main.kcl")).unwrap();
        let source = "@settings(kclVersion = 3.0)\nimport size as measure from \"dims.kcl\"\nx = measure(1mm, height = 3mm, depth = 4mm)\n";
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
        let result = server
            .signature_help(SignatureHelpParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: kcl_lib::byte_to_lsp_position(source, source.find("4mm").unwrap()).unwrap(),
                },
                context: None,
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.active_parameter, Some(1));
        assert!(result.signatures[0].label.starts_with("measure("));
        assert!(result.signatures[0].label.contains("@width: number(mm)"));
        assert!(result.signatures[0].documentation.as_ref().is_some_and(|docs| matches!(docs, Documentation::MarkupContent(content) if content.value.contains("Cabinet dimensions."))));
    }
}
