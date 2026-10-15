//! AST-based lexical navigation, independent of execution and engine availability.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashSet;

use tower_lsp::lsp_types::GotoDefinitionResponse;
use tower_lsp::lsp_types::Location;
use tower_lsp::lsp_types::Position;
use tower_lsp::lsp_types::Range;
use tower_lsp::lsp_types::TextDocumentPositionParams;
use tower_lsp::lsp_types::Url;

use super::Backend;
use crate::Program;
use crate::SourceRange;
use crate::parsing::ast::types::Expr;
use crate::parsing::ast::types::ImportPath;
use crate::parsing::ast::types::ImportSelector;
use crate::parsing::ast::types::ItemVisibility;
use crate::parsing::ast::types::Name;
use crate::parsing::ast::types::PrimitiveType;
use crate::parsing::ast::types::Type;
use crate::parsing::ast::types::TypeDeclarationDefinition;
use crate::walk::Node;

#[derive(Clone)]
pub(super) struct Binding {
    pub(super) name: String,
    pub(super) span: SourceRange,
    pub(super) scope: SourceRange,
    pub(super) exported: bool,
    pub(super) import: Option<(Url, Option<String>)>,
    pub(super) alias: bool,
}

pub(super) struct Occurrence {
    pub(super) name: String,
    pub(super) qualifier: Option<String>,
    pub(super) span: SourceRange,
    pub(super) declaration: bool,
    pub(super) import_name: bool,
    pub(super) callee: Option<(String, Option<String>)>,
    pub(super) type_context: bool,
}

pub(super) struct Document {
    pub(super) source: String,
    pub(super) bindings: Vec<Binding>,
    pub(super) occurrences: Vec<Occurrence>,
    pub(super) imports: Vec<Url>,
    pub(super) import_links: Vec<(SourceRange, Url)>,
    pub(super) globs: Vec<(Url, SourceRange, bool)>,
    pub(super) functions: Vec<(SourceRange, SourceRange)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Symbol {
    pub(super) uri: Url,
    pub(super) start: usize,
    pub(super) end: usize,
}

pub(super) type Index = BTreeMap<Url, Document>;

fn name_occurrences(value: &Name, occurrences: &RefCell<Vec<Occurrence>>, type_context: bool) {
    if value.abs_path {
        return;
    }
    let mut path = vec![];
    for name in value.path.iter().chain(std::iter::once(&value.name)) {
        occurrences.borrow_mut().push(Occurrence {
            name: name.name.clone(),
            qualifier: (!path.is_empty()).then(|| path.join("::")),
            span: SourceRange::from(name),
            declaration: false,
            import_name: false,
            callee: None,
            type_context,
        });
        path.push(name.name.clone());
    }
}

fn type_occurrences(ty: &Type, occurrences: &RefCell<Vec<Occurrence>>) {
    match ty {
        Type::Named { name } => name_occurrences(name, occurrences, true),
        Type::Array { ty, .. } => type_occurrences(ty, occurrences),
        Type::Union { tys } => {
            for ty in tys {
                type_occurrences(ty, occurrences);
            }
        }
        Type::Object { properties } => {
            for (_, ty) in properties {
                type_occurrences(ty, occurrences);
            }
        }
        Type::Primitive(PrimitiveType::Function(function)) => {
            for ty in function.unnamed_arg.iter().chain(function.return_type.iter()) {
                type_occurrences(ty, occurrences);
            }
            for (_, ty) in &function.named_args {
                type_occurrences(ty, occurrences);
            }
        }
        Type::Primitive(_) => {}
    }
}

impl Document {
    pub(super) fn parse(uri: &Url, source: String) -> Option<Self> {
        let (program, issues) = Program::parse(&source).ok()?;
        if issues.iter().any(|issue| issue.is_err()) {
            return None;
        }
        let program = program?;
        let scopes = RefCell::new(vec![SourceRange::new(0, source.len(), Default::default())]);
        crate::walk::walk(&program.ast, |node| {
            match node {
                Node::Program(value) => scopes.borrow_mut().push(SourceRange::from(value)),
                Node::Block(value) => scopes.borrow_mut().push(SourceRange::from(value)),
                _ => {}
            }
            Ok::<_, anyhow::Error>(true)
        })
        .ok()?;
        let scope = |span: SourceRange| {
            scopes
                .borrow()
                .iter()
                .filter(|scope| scope.start() <= span.start() && scope.end() >= span.end())
                .min_by_key(|scope| scope.end() - scope.start())
                .copied()
                .unwrap_or_default()
        };
        let bindings = RefCell::new(Vec::new());
        let occurrences = RefCell::new(Vec::new());
        let imports = RefCell::new(Vec::new());
        let import_links = RefCell::new(Vec::new());
        let globs = RefCell::new(Vec::new());
        let excluded = RefCell::new(Vec::new());
        let functions = RefCell::new(Vec::new());
        let add = |name: String,
                   span: SourceRange,
                   scope: SourceRange,
                   exported: bool,
                   import: Option<(Url, Option<String>)>,
                   alias: bool| {
            occurrences.borrow_mut().push(Occurrence {
                name: name.clone(),
                qualifier: None,
                span,
                declaration: import.is_none(),
                import_name: false,
                callee: None,
                type_context: false,
            });
            bindings.borrow_mut().push(Binding {
                name,
                span,
                scope,
                exported,
                import,
                alias,
            });
        };
        crate::walk::walk(&program.ast, |node| {
            match node {
                Node::VariableDeclaration(value) => {
                    let span = SourceRange::from(&value.declaration.id);
                    if let Expr::FunctionExpression(function) = &value.declaration.init {
                        functions.borrow_mut().push((span, SourceRange::from(function)));
                    }
                    add(
                        value.declaration.id.name.clone(),
                        span,
                        scope(span),
                        value.visibility == ItemVisibility::Export,
                        None,
                        false,
                    );
                }
                Node::TagDeclarator(value) => {
                    let span = SourceRange::from(value);
                    add(value.name.clone(), span, scope(span), false, None, false);
                }
                Node::TypeDeclaration(value) => {
                    if let TypeDeclarationDefinition::Alias { ty } = &value.definition {
                        type_occurrences(ty, &occurrences);
                    }
                    if let Some(args) = &value.args {
                        for arg in args {
                            add(
                                arg.name.clone(),
                                SourceRange::from(arg),
                                SourceRange::from(value),
                                false,
                                None,
                                false,
                            );
                        }
                    }
                    let span = SourceRange::from(&value.name);
                    add(
                        value.name.name.clone(),
                        span,
                        scope(span),
                        value.visibility == ItemVisibility::Export,
                        None,
                        false,
                    );
                }
                Node::FunctionExpression(value) => {
                    if let Some(ty) = &value.return_type {
                        type_occurrences(ty, &occurrences);
                    }
                    for param in &value.params {
                        add(
                            param.identifier.name.clone(),
                            SourceRange::from(&param.identifier),
                            SourceRange::from(value),
                            false,
                            None,
                            false,
                        );
                    }
                }
                Node::ImportStatement(value) => {
                    let ImportPath::Kcl { filename } = &value.path else {
                        return Ok(true);
                    };
                    let Ok(target) = uri.join(
                        &filename
                            .to_string()
                            .replace('%', "%25")
                            .replace('#', "%23")
                            .replace('?', "%3F"),
                    ) else {
                        return Ok(true);
                    };
                    imports.borrow_mut().push(target.clone());
                    import_links
                        .borrow_mut()
                        .push((SourceRange::from(value), target.clone()));
                    let exported = value.visibility == ItemVisibility::Export;
                    match &value.selector {
                        ImportSelector::List { items } => {
                            for item in items {
                                let id = item.alias.as_ref().unwrap_or(&item.name);
                                let span = SourceRange::from(id);
                                add(
                                    id.name.clone(),
                                    span,
                                    scope(span),
                                    exported,
                                    Some((target.clone(), Some(item.name.name.clone()))),
                                    item.alias.is_some(),
                                );
                                if item.alias.is_some() {
                                    occurrences.borrow_mut().push(Occurrence {
                                        name: id.name.clone(),
                                        qualifier: None,
                                        span: SourceRange::from(&item.name),
                                        declaration: false,
                                        import_name: true,
                                        callee: None,
                                        type_context: false,
                                    });
                                }
                            }
                        }
                        ImportSelector::None { alias } => {
                            let name = alias.as_ref().map(|id| id.name.clone()).unwrap_or_else(|| {
                                filename
                                    .to_string()
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or_default()
                                    .trim_end_matches(".kcl")
                                    .to_owned()
                            });
                            let span = alias
                                .as_ref()
                                .map(SourceRange::from)
                                .unwrap_or_else(|| SourceRange::from(value));
                            add(name, span, scope(span), exported, Some((target, None)), alias.is_some());
                        }
                        ImportSelector::Glob(_) => {
                            globs
                                .borrow_mut()
                                .push((target, scope(SourceRange::from(value)), exported))
                        }
                    }
                }
                Node::Name(value) => name_occurrences(value, &occurrences, false),
                Node::Parameter(value) => {
                    if let Some(ty) = &value.param_type {
                        type_occurrences(ty, &occurrences);
                    }
                }
                Node::AscribedExpression(value) => type_occurrences(&value.ty, &occurrences),
                Node::CallExpressionKw(value) if !value.callee.abs_path => {
                    for arg in &value.arguments {
                        if let Some(label) = &arg.label {
                            occurrences.borrow_mut().push(Occurrence {
                                name: label.name.clone(),
                                qualifier: None,
                                span: SourceRange::from(label),
                                declaration: false,
                                import_name: false,
                                type_context: false,
                                callee: Some((
                                    value.callee.name.name.clone(),
                                    (!value.callee.path.is_empty()).then(|| {
                                        value
                                            .callee
                                            .path
                                            .iter()
                                            .map(|name| name.name.as_str())
                                            .collect::<Vec<_>>()
                                            .join("::")
                                    }),
                                )),
                            });
                        }
                    }
                }
                Node::MemberExpression(value) if !value.computed => {
                    excluded.borrow_mut().push(SourceRange::from(&value.property));
                    if let (Expr::Name(object), Expr::Name(property)) = (&value.object, &value.property) {
                        occurrences.borrow_mut().push(Occurrence {
                            name: property.name.name.clone(),
                            qualifier: Some(
                                object
                                    .path
                                    .iter()
                                    .chain(std::iter::once(&object.name))
                                    .map(|name| name.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join("::"),
                            ),
                            span: SourceRange::from(&property.name),
                            declaration: false,
                            import_name: false,
                            callee: None,
                            type_context: false,
                        });
                    }
                }
                _ => {}
            }
            Ok::<_, anyhow::Error>(true)
        })
        .ok()?;
        let mut occurrences = occurrences.into_inner();
        occurrences.retain(|occurrence| {
            occurrence.qualifier.is_some()
                || !excluded
                    .borrow()
                    .iter()
                    .any(|span| span.start() <= occurrence.span.start() && span.end() >= occurrence.span.end())
        });
        Some(Self {
            source,
            bindings: bindings.into_inner(),
            occurrences,
            imports: imports.into_inner(),
            import_links: import_links.into_inner(),
            globs: globs.into_inner(),
            functions: functions.into_inner(),
        })
    }

    fn binding(&self, name: &str, offset: usize) -> Option<&Binding> {
        self.bindings
            .iter()
            .filter(|binding| binding.name == name && binding.scope.start() <= offset && offset <= binding.scope.end())
            .min_by_key(|binding| binding.scope.end() - binding.scope.start())
    }
}

fn resolve(
    index: &Index,
    uri: &Url,
    name: &str,
    offset: usize,
    exported: bool,
    follow_aliases: bool,
    visited: &mut HashSet<(Url, String)>,
) -> Option<Symbol> {
    if !visited.insert((uri.clone(), name.to_owned())) {
        return None;
    }
    let document = index.get(uri)?;
    let binding = if exported {
        document
            .bindings
            .iter()
            .find(|binding| binding.name == name && binding.exported)
    } else {
        document.binding(name, offset)
    };
    if let Some(binding) = binding {
        if let Some((target, Some(name))) = &binding.import
            && (follow_aliases || !binding.alias)
        {
            return resolve(index, target, name, 0, true, follow_aliases, visited);
        }
        return Some(Symbol {
            uri: uri.clone(),
            start: binding.span.start(),
            end: binding.span.end(),
        });
    }
    // An ambiguous glob must not point arbitrarily at one of several exports.
    let mut candidates = document
        .globs
        .iter()
        .filter(|(_, scope, public)| {
            (!exported || *public) && (exported || (scope.start() <= offset && offset <= scope.end()))
        })
        .filter_map(|(target, _, _)| resolve(index, target, name, 0, true, follow_aliases, &mut visited.clone()));
    let first = candidates.next()?;
    if candidates.any(|candidate| candidate != first) {
        return None;
    }
    Some(first)
}

fn occurrence_symbol(index: &Index, uri: &Url, occurrence: &Occurrence) -> Option<Symbol> {
    resolve_occurrence(index, uri, occurrence, true)
}

// Navigation follows aliases to their definitions. Rename preserves explicit
// aliases as separate bindings.
pub(super) fn resolve_occurrence(
    index: &Index,
    uri: &Url,
    occurrence: &Occurrence,
    follow_aliases: bool,
) -> Option<Symbol> {
    if let Some((name, qualifier)) = &occurrence.callee {
        let call = Occurrence {
            name: name.clone(),
            qualifier: qualifier.clone(),
            span: occurrence.span,
            declaration: false,
            import_name: false,
            callee: None,
            type_context: false,
        };
        let function = resolve_occurrence(index, uri, &call, true)?;
        let doc = index.get(&function.uri)?;
        let (_, scope) = doc
            .functions
            .iter()
            .find(|(span, _)| span.start() == function.start && span.end() == function.end)?;
        let parameter = doc
            .bindings
            .iter()
            .find(|binding| binding.scope == *scope && binding.name == occurrence.name)?;
        return Some(Symbol {
            uri: function.uri,
            start: parameter.span.start(),
            end: parameter.span.end(),
        });
    }
    if occurrence.import_name {
        let binding = index.get(uri)?.binding(&occurrence.name, occurrence.span.start())?;
        let (target, Some(name)) = binding.import.as_ref()? else {
            return None;
        };
        return resolve(index, target, name, 0, true, follow_aliases, &mut HashSet::new());
    }
    if let Some(namespace) = &occurrence.qualifier {
        let mut names = namespace.split("::");
        let binding = index.get(uri)?.binding(names.next()?, occurrence.span.start())?;
        let (target, None) = binding.import.as_ref()? else {
            return None;
        };
        let mut target = target;
        for name in names {
            let symbol = resolve(index, target, name, 0, true, true, &mut HashSet::new())?;
            let binding = index
                .get(&symbol.uri)?
                .bindings
                .iter()
                .find(|binding| binding.span.start() == symbol.start && binding.span.end() == symbol.end)?;
            let (next, None) = binding.import.as_ref()? else {
                return None;
            };
            target = next;
        }
        return resolve(
            index,
            target,
            &occurrence.name,
            0,
            true,
            follow_aliases,
            &mut HashSet::new(),
        );
    }

    resolve(
        index,
        uri,
        &occurrence.name,
        occurrence.span.start(),
        false,
        follow_aliases,
        &mut HashSet::new(),
    )
}

fn byte_offset(source: &str, position: Position) -> Option<usize> {
    kcl_lib::lsp_position_to_byte(source, position)
}

fn position(source: &str, offset: usize) -> Option<Position> {
    kcl_lib::byte_to_lsp_position(source, offset)
}

fn location(index: &Index, symbol: &Symbol) -> Option<Location> {
    let source = &index.get(&symbol.uri)?.source;
    Some(Location {
        uri: symbol.uri.clone(),
        range: Range::new(position(source, symbol.start)?, position(source, symbol.end)?),
    })
}

pub(super) fn document_path(uri: &Url) -> Option<kcl_lib::TypedPath> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(kcl_lib::TypedPath::new(&uri.to_file_path().ok()?.to_string_lossy()))
    }
    #[cfg(target_arch = "wasm32")]
    {
        Some(kcl_lib::TypedPath::new(
            &percent_encoding::percent_decode_str(uri.path()).decode_utf8().ok()?,
        ))
    }
}

impl Backend {
    async fn navigation_index(&self, uri: &Url, workspace: bool) -> Index {
        self.navigation_index_with_errors(uri, workspace).await.0
    }

    pub(super) async fn navigation_index_with_errors(&self, uri: &Url, workspace: bool) -> (Index, Vec<Url>) {
        let mut failed = vec![];
        let mut pending = vec![uri.clone()];
        if workspace {
            #[cfg(not(target_arch = "wasm32"))]
            if let Ok(path) = uri.to_file_path() {
                let root = self.document_project_root(&path);
                let mut directories = if root.parent().is_some() { vec![root] } else { vec![] };
                while let Some(directory) = directories.pop() {
                    let Ok(entries) = std::fs::read_dir(directory) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        let Ok(kind) = entry.file_type() else {
                            continue;
                        };
                        if kind.is_dir()
                            && ![".git", ".jj", "node_modules", "target"]
                                .contains(&entry.file_name().to_string_lossy().as_ref())
                        {
                            directories.push(entry.path());
                        } else if kind.is_file()
                            && entry.path().extension().is_some_and(|ext| ext == "kcl")
                            && let Ok(uri) = Url::from_file_path(entry.path())
                        {
                            pending.push(uri);
                        }
                    }
                }
            }
            pending.extend(
                self.open_documents
                    .iter()
                    .filter_map(|entry| Url::parse(entry.key()).ok()),
            );
        }
        let mut index = BTreeMap::new();
        let mut visited = HashSet::new();
        while let Some(uri) = pending.pop() {
            if !visited.insert(uri.clone()) {
                continue;
            }
            let source = if let Some(contents) = self
                .code_map
                .get(uri.as_str())
                .filter(|_| self.open_documents.contains_key(uri.as_str()) || uri.scheme() != "file")
            {
                String::from_utf8(contents.clone()).ok()
            } else if let Some(path) = document_path(&uri) {
                self.fs.read_to_string(&path, Default::default()).await.ok()
            } else {
                None
            };
            if let Some(document) = source.and_then(|source| Document::parse(&uri, source)) {
                pending.extend(document.imports.iter().cloned());
                index.insert(uri, document);
            } else {
                failed.push(uri);
            }
        }
        (index, failed)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) async fn document_imports(&self, document: &Url, dependency: &Url) -> bool {
        self.navigation_index(document, false)
            .await
            .values()
            .any(|document| document.imports.contains(dependency))
    }

    fn navigation_symbol(index: &Index, params: &TextDocumentPositionParams) -> Option<Symbol> {
        let uri = &params.text_document.uri;
        let document = index.get(uri)?;
        let offset = byte_offset(&document.source, params.position)?;
        let occurrence = document
            .occurrences
            .iter()
            .filter(|item| item.span.start() <= offset && offset < item.span.end())
            .min_by_key(|item| item.span.end() - item.span.start())?;
        occurrence_symbol(index, uri, occurrence)
    }

    pub(super) async fn navigation_definition(
        &self,
        params: TextDocumentPositionParams,
    ) -> Option<GotoDefinitionResponse> {
        let index = self.navigation_index(&params.text_document.uri, false).await;
        let mut symbol = if let Some(symbol) = Self::navigation_symbol(&index, &params) {
            symbol
        } else {
            let document = index.get(&params.text_document.uri)?;
            let offset = byte_offset(&document.source, params.position)?;
            let (_, target) = document
                .import_links
                .iter()
                .find(|(range, _)| range.start() <= offset && offset < range.end())?;
            return Some(GotoDefinitionResponse::Scalar(Location {
                uri: target.clone(),
                range: Range::default(),
            }));
        };
        // Selecting a module binding opens the imported module, rather than
        // jumping back to its local import alias.
        if let Some(binding) = index
            .get(&symbol.uri)?
            .bindings
            .iter()
            .find(|binding| binding.span.start() == symbol.start)
            && let Some((target, None)) = &binding.import
        {
            symbol = Symbol {
                uri: target.clone(),
                start: 0,
                end: 0,
            };
        }
        location(&index, &symbol).map(GotoDefinitionResponse::Scalar)
    }

    pub(super) async fn navigation_references(
        &self,
        params: TextDocumentPositionParams,
        include_declaration: bool,
    ) -> Option<Vec<Location>> {
        let index = self.navigation_index(&params.text_document.uri, true).await;
        let symbol = Self::navigation_symbol(&index, &params)?;
        let mut locations = Vec::new();
        for (uri, document) in &index {
            for occurrence in &document.occurrences {
                if (!occurrence.declaration || include_declaration)
                    && occurrence_symbol(&index, uri, occurrence).as_ref() == Some(&symbol)
                    && let Some(location) = location(
                        &index,
                        &Symbol {
                            uri: uri.clone(),
                            start: occurrence.span.start(),
                            end: occurrence.span.end(),
                        },
                    )
                {
                    locations.push(location);
                }
            }
        }
        locations.sort_by(|a, b| {
            a.uri
                .as_str()
                .cmp(b.uri.as_str())
                .then(a.range.start.cmp(&b.range.start))
        });
        locations.dedup();
        Some(locations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(index: &mut Index, filename: &str, source: &str) -> Url {
        let uri = Url::parse(&format!("file:///project/{filename}")).unwrap();
        index.insert(uri.clone(), Document::parse(&uri, source.into()).unwrap());
        uri
    }

    fn symbol_at(index: &Index, uri: &Url, needle: &str) -> Symbol {
        let source = &index[uri].source;
        let offset = source.find(needle).unwrap();
        Backend::navigation_symbol(
            index,
            &TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: position(source, offset).unwrap(),
            },
        )
        .unwrap()
    }

    #[test]
    fn navigation_respects_shadowing_and_ignores_comments_and_keys() {
        let mut index = Index::new();
        let uri = document(
            &mut index,
            "main.kcl",
            "value = 1\nfn f(@value) {\n  return value + 2\n}\nresult = value\nobj = { value = value }\n// value\n",
        );
        let global = symbol_at(&index, &uri, "value = 1");
        let parameter = symbol_at(&index, &uri, "value) {");
        assert_ne!(global, parameter);
        assert_eq!(symbol_at(&index, &uri, "value + 2"), parameter);
        assert_eq!(symbol_at(&index, &uri, "value\nobj"), global);
        let doc = &index[&uri];
        let references: Vec<_> = doc
            .occurrences
            .iter()
            .filter(|occurrence| occurrence_symbol(&index, &uri, occurrence).as_ref() == Some(&global))
            .collect();
        assert_eq!(references.len(), 3); // declaration, result, object value
    }

    #[test]
    fn navigation_follows_aliases_reexports_and_namespace_members() {
        let mut index = Index::new();
        let source = document(&mut index, "dims.kcl", "export width = 7\nprivate = 9\n");
        document(
            &mut index,
            "reexport.kcl",
            "export import width as size from \"dims.kcl\"\n",
        );
        let main = document(
            &mut index,
            "main.kcl",
            "import size as w from \"reexport.kcl\"\nimport \"dims.kcl\" as dims\na = w\nb = dims.width\nc = dims::width\n",
        );
        let width = symbol_at(&index, &source, "width =");
        assert_eq!(symbol_at(&index, &main, "w\nb"), width);
        assert_eq!(symbol_at(&index, &main, "width\nc"), width);
        assert_eq!(symbol_at(&index, &main, "width\n"), width);
    }

    #[test]
    fn navigation_globs_require_unambiguous_exports_and_handle_cycles() {
        let mut index = Index::new();
        let a = document(&mut index, "a.kcl", "export width = 7\n");
        document(&mut index, "b.kcl", "export width = 8\n");
        let main = document(&mut index, "main.kcl", "import * from \"a.kcl\"\nx = width\n");
        assert_eq!(symbol_at(&index, &main, "width\n"), symbol_at(&index, &a, "width ="));
        document(
            &mut index,
            "main.kcl",
            "import * from \"a.kcl\"\nimport * from \"b.kcl\"\nx = width\n",
        );
        assert!(
            resolve(
                &index,
                &main,
                "width",
                index[&main].source.len() - 2,
                false,
                true,
                &mut HashSet::new()
            )
            .is_none()
        );
        document(&mut index, "cycle.kcl", "export import width from \"cycle.kcl\"\n");
        assert!(
            resolve(
                &index,
                &Url::parse("file:///project/cycle.kcl").unwrap(),
                "width",
                0,
                true,
                true,
                &mut HashSet::new()
            )
            .is_none()
        );
    }

    #[test]
    fn navigation_positions_use_utf16_and_reject_invalid_offsets() {
        let source = "// 😀\nvalue = 1\nresult = value";
        for offset in [0, 3, 7, source.len()] {
            let pos = position(source, offset).unwrap();
            assert_eq!(byte_offset(source, pos), Some(offset));
        }
        assert_eq!(byte_offset(source, Position::new(0, 4)), None); // inside surrogate pair
        assert_eq!(byte_offset(source, Position::new(30, 0)), None);
    }
}
