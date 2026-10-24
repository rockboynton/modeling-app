//! Conservative source types for editor assistance; never executes user code.
use std::cell::RefCell;
use std::collections::BTreeMap;

use tower_lsp::lsp_types::*;

use super::navigation::Index;
use super::navigation::Occurrence;
use super::navigation::resolve_occurrence;
use crate::Program;
use crate::SourceRange;
use crate::parsing::ast::types::Expr;
use crate::parsing::ast::types::LiteralValue;
use crate::parsing::ast::types::Name;
use crate::parsing::ast::types::Type;
use crate::parsing::ast::types::TypeDeclarationDefinition;
use crate::walk::Node;

#[derive(Clone, Default)]
pub(super) struct Shape {
    pub label: String,
    pub fields: BTreeMap<String, Shape>,
    pub element: Option<Box<Shape>>,
}

fn common(left: Shape, right: Shape) -> Option<Shape> {
    if left.label != right.label {
        return None;
    }
    let fields = left
        .fields
        .into_iter()
        .filter_map(|(name, shape)| {
            let other = right.fields.get(&name)?.clone();
            Some((name, common(shape, other)?))
        })
        .collect();
    let element = match (left.element, right.element) {
        (Some(left), Some(right)) => common(*left, *right).map(Box::new),
        _ => None,
    };
    Some(Shape {
        label: left.label,
        fields,
        element,
    })
}

fn named(index: &Index, uri: &Url, name: &Name, type_context: bool, depth: usize) -> Option<Shape> {
    let occurrence = Occurrence {
        name: name.name.name.clone(),
        qualifier: (!name.path.is_empty())
            .then(|| name.path.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join("::")),
        span: SourceRange::from(&name.name),
        declaration: false,
        import_name: false,
        callee: None,
        type_context,
    };
    let symbol = resolve_occurrence(index, uri, &occurrence, true)?;
    let document = index.get(&symbol.uri)?;
    let program = Program::parse_no_errs(&document.source).ok()?;
    let result = RefCell::new(None);
    crate::walk::walk(&program.ast, |node| {
        match node {
            Node::VariableDeclaration(value)
                if value.declaration.id.start == symbol.start && value.declaration.id.end == symbol.end =>
            {
                result.replace(expression(index, &symbol.uri, &value.declaration.init, depth + 1));
            }
            Node::Parameter(value) if value.identifier.start == symbol.start && value.identifier.end == symbol.end => {
                result.replace(
                    value
                        .param_type
                        .as_ref()
                        .and_then(|ty| annotation(index, &symbol.uri, ty, depth + 1)),
                );
            }
            Node::TypeDeclaration(value) if value.name.start == symbol.start && value.name.end == symbol.end => {
                if let TypeDeclarationDefinition::Alias { ty } = &value.definition {
                    result.replace(annotation(index, &symbol.uri, ty, depth + 1));
                }
            }
            _ => {}
        }
        Ok::<_, anyhow::Error>(true)
    })
    .ok()?;
    result.into_inner()
}

fn annotation(index: &Index, uri: &Url, ty: &Type, depth: usize) -> Option<Shape> {
    if depth > 16 {
        return None;
    }
    let mut shape = Shape {
        label: ty.to_string(),
        ..Default::default()
    };
    match ty {
        Type::Named { name } => return named(index, uri, name, true, depth),
        Type::Object { properties } => {
            shape.fields = properties
                .iter()
                .filter_map(|(name, ty)| Some((name.name.clone(), annotation(index, uri, ty, depth + 1)?)))
                .collect();
        }
        Type::Array { ty, .. } => shape.element = annotation(index, uri, ty, depth + 1).map(Box::new),
        _ => {}
    }
    Some(shape)
}

pub(super) fn expression(index: &Index, uri: &Url, expr: &Expr, depth: usize) -> Option<Shape> {
    if depth > 16 {
        return None;
    }
    let scalar = |label: String| {
        Some(Shape {
            label,
            ..Default::default()
        })
    };
    match expr {
        Expr::Name(name) => named(index, uri, name, false, depth),
        Expr::AscribedExpression(value) => annotation(index, uri, &value.ty, depth + 1),
        Expr::Literal(value) => match &value.value {
            LiteralValue::String(_) => scalar("string".into()),
            LiteralValue::Bool(_) => scalar("bool".into()),
            LiteralValue::Number { suffix, .. } => scalar(format!(
                "number({})",
                if suffix.to_string().is_empty() {
                    "_".into()
                } else {
                    suffix.to_string()
                }
            )),
        },
        Expr::ObjectExpression(value) => Some(Shape {
            label: "object".into(),
            fields: value
                .properties
                .iter()
                .map(|p| {
                    (
                        p.key.name.clone(),
                        expression(index, uri, &p.value, depth + 1).unwrap_or_default(),
                    )
                })
                .collect(),
            element: None,
        }),
        Expr::ArrayExpression(value) => {
            let mut elements = value.elements.iter();
            let first = expression(index, uri, elements.next()?, depth + 1)?;
            let element = elements.try_fold(first, |previous, item| {
                common(previous, expression(index, uri, item, depth + 1)?)
            })?;
            Some(Shape {
                label: format!("{}[]", element.label),
                element: Some(Box::new(element)),
                ..Default::default()
            })
        }
        Expr::MemberExpression(value) => {
            let object = expression(index, uri, &value.object, depth + 1)?;
            if value.computed {
                if let Expr::Literal(key) = &value.property
                    && let LiteralValue::String(key) = &key.value
                {
                    return object.fields.get(key).cloned();
                }
                object.element.map(|v| *v)
            } else if let Expr::Name(name) = &value.property {
                object.fields.get(&name.name.name).cloned()
            } else {
                None
            }
        }
        Expr::CallExpressionKw(call) => {
            let occurrence = Occurrence {
                name: call.callee.name.name.clone(),
                qualifier: (!call.callee.path.is_empty()).then(|| {
                    call.callee
                        .path
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join("::")
                }),
                span: SourceRange::from(&call.callee.name),
                declaration: false,
                import_name: false,
                callee: None,
                type_context: false,
            };
            let symbol = resolve_occurrence(index, uri, &occurrence, true)?;
            let function = super::calls::function(index, uri, &occurrence)?;
            annotation(index, &symbol.uri, function.ast.return_type.as_ref()?, depth + 1)
        }
        Expr::UnaryExpression(value) => expression(index, uri, &Expr::from(&value.argument), depth + 1),
        Expr::BinaryExpression(value) => {
            let left = expression(index, uri, &Expr::from(&value.left), depth + 1)?;
            let right = expression(index, uri, &Expr::from(&value.right), depth + 1)?;
            // Preserve units only where the operator cannot change them.
            let op = value.operator.to_string();
            if (left.label == right.label && matches!(op.as_str(), "+" | "-"))
                || (right.label == "number(_)" && matches!(op.as_str(), "*" | "/"))
            {
                Some(left)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(super) fn member_fields(index: &Index, uri: &Url, offset: usize) -> Option<BTreeMap<String, Shape>> {
    let document = index.get(uri)?;
    let program = Program::parse_no_errs(&document.source).ok()?;
    let result = RefCell::new(None);
    crate::walk::walk(&program.ast, |node| {
        if let Node::MemberExpression(member) = node
            && !member.computed
            && member.property.start() <= offset
            && offset <= member.property.end()
        {
            result.replace(expression(index, uri, &member.object, 0).map(|s| s.fields));
        }
        Ok::<_, anyhow::Error>(true)
    })
    .ok()?;
    result.into_inner()
}

#[cfg(test)]
mod tests {
    use tower_lsp::LanguageServer;

    use super::*;

    #[tokio::test]
    async fn member_completion_resolves_literals_parameters_returns_and_array_elements() {
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let uri = Url::parse("file:///member-shapes.kcl").unwrap();
        for body in [
            "config = {inner = {width = 20mm}}\nvalue = config.inner.|",
            "items = [{width = 20mm}]\nvalue = items[0].|",
            "fn f(@dims: {width: number(mm)}) { return dims.| }",
            "fn make(): {width: number(mm)} { return {width = 20mm} }\nvalue = make().|",
        ] {
            let offset = body.find('|').unwrap() + "@settings(kclVersion = 3.0)\n".len();
            let source = format!("@settings(kclVersion = 3.0)\n{}", body.replace('|', ""));
            server
                .did_open(DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "kcl".into(),
                        version: 1,
                        text: source.clone(),
                    },
                })
                .await;
            let items = server.lexical_completions(&uri, &source, offset).await.unwrap();
            assert_eq!(items.items.len(), 1, "{body}: {:?}", items.items);
            assert_eq!(items.items[0].label, "width", "{body}");
            assert_eq!(items.items[0].detail.as_deref(), Some("number(mm)"), "{body}");
            assert_eq!(items.items[0].kind, Some(CompletionItemKind::FIELD));
        }
    }

    #[tokio::test]
    async fn member_completion_follows_imported_type_aliases_and_unsaved_buffers() {
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let header = "@settings(kclVersion = 3.0, experimentalFeatures = allow)\n";
        let types = format!("{header}export type Dimensions = {{width: number(mm)}}\n");
        Program::parse_no_errs(&types).unwrap();
        std::fs::write(dir.path().join("types.kcl"), &types).unwrap();
        let uri = Url::from_file_path(dir.path().join("main.kcl")).unwrap();
        let source = format!("{header}import Dimensions as D from \"types.kcl\"\nfn f(@dims: D) {{ return dims. }}");
        let offset = source.find("dims. }").unwrap() + 5;
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "kcl".into(),
                    version: 1,
                    text: source.clone(),
                },
            })
            .await;
        assert_eq!(
            server.lexical_completions(&uri, &source, offset).await.unwrap().items[0].label,
            "width"
        );
        let imported = Url::from_file_path(dir.path().join("types.kcl")).unwrap();
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: imported,
                    language_id: "kcl".into(),
                    version: 1,
                    text: types.replace("width", "depth"),
                },
            })
            .await;
        assert_eq!(
            server.lexical_completions(&uri, &source, offset).await.unwrap().items[0].label,
            "depth"
        );
    }
    #[test]
    fn array_field_inference_only_offers_fields_shared_by_every_element() {
        let uri = Url::parse("file:///array-fields.kcl").unwrap();
        let source = "@settings(kclVersion = 3.0)\nitems = [{width = 2mm, depth = 3mm}, {width = 4mm, height = 5mm}]\nvalue = items[0].width\n";
        let mut index = Index::new();
        index.insert(
            uri.clone(),
            super::super::navigation::Document::parse(&uri, source.into()).unwrap(),
        );
        let fields = member_fields(&index, &uri, source.rfind("width").unwrap()).unwrap();
        assert_eq!(fields.keys().cloned().collect::<Vec<_>>(), ["width"]);
    }
}
