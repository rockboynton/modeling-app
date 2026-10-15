//! Binding-aware rename with targeted edits and collision checks.
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use tower_lsp::jsonrpc::Error;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::DocumentChanges;
use tower_lsp::lsp_types::OneOf;
use tower_lsp::lsp_types::OptionalVersionedTextDocumentIdentifier;
use tower_lsp::lsp_types::PrepareRenameResponse;
use tower_lsp::lsp_types::Range;
use tower_lsp::lsp_types::TextDocumentEdit;
use tower_lsp::lsp_types::TextDocumentPositionParams;
use tower_lsp::lsp_types::TextEdit;
use tower_lsp::lsp_types::Url;
use tower_lsp::lsp_types::WorkspaceEdit;

use super::Backend;
use super::navigation::Document;
use super::navigation::Index;
use super::navigation::Occurrence;
use super::navigation::Symbol;
use super::navigation::resolve_occurrence;

type Replacements = BTreeMap<Url, Vec<(usize, usize)>>;

fn occurrence_at<'a>(index: &'a Index, params: &TextDocumentPositionParams) -> Option<&'a Occurrence> {
    let document = index.get(&params.text_document.uri)?;
    let offset = kcl_lib::lsp_position_to_byte(&document.source, params.position)?;
    document
        .occurrences
        .iter()
        .filter(|item| item.span.start() <= offset && offset < item.span.end())
        .min_by_key(|item| item.span.end() - item.span.start())
}

fn renamed_offset(replacements: &Replacements, uri: &Url, offset: usize, name: &str) -> usize {
    let mut mapped = offset;
    for &(start, end) in replacements.get(uri).into_iter().flatten() {
        if end <= offset {
            mapped = mapped - (end - start) + name.len();
        }
    }
    mapped
}

fn renamed_symbol(replacements: &Replacements, symbol: Symbol, name: &str) -> Symbol {
    Symbol {
        start: renamed_offset(replacements, &symbol.uri, symbol.start, name),
        end: renamed_offset(replacements, &symbol.uri, symbol.end, name),
        uri: symbol.uri,
    }
}

fn rename_edits(index: &Index, symbol: &Symbol, name: &str) -> Result<WorkspaceEdit> {
    let definition = index.get(&symbol.uri).ok_or_else(Error::content_modified)?;
    let old_name = &definition.source[symbol.start..symbol.end];
    // A lexical parameter can shadow a type in annotations evaluated in the
    // enclosing environment. Refuse these ambiguous cases until type checking
    // supplies authoritative binding identities.
    for (uri, document) in index {
        for occurrence in document
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.type_context && occurrence.name == old_name)
        {
            let resolved = resolve_occurrence(index, uri, occurrence, false)
                .ok_or_else(|| Error::invalid_params("Cannot safely resolve a type reference for this rename"))?;
            let target = &index[&resolved.uri];
            if let Some(binding) = target
                .bindings
                .iter()
                .find(|binding| binding.span.start() == resolved.start && binding.span.end() == resolved.end)
                && target.functions.iter().any(|(_, scope)| *scope == binding.scope)
            {
                return Err(Error::invalid_params(
                    "Cannot safely rename a name that shadows a type annotation",
                ));
            }
        }
    }

    let validation_uri: Url = "file:///rename-validation.kcl"
        .parse()
        .map_err(|_| Error::internal_error())?;
    let validation = Document::parse(&validation_uri, format!("@settings(kclVersion = 3.0)\n{name} = 0\n"));
    if !validation.is_some_and(|doc| {
        doc.bindings.len() == 1
            && doc.bindings[0].name == name
            && doc.bindings[0].span.end() - doc.bindings[0].span.start() == name.len()
    }) {
        return Err(Error::invalid_params("The new name must be a valid KCL identifier"));
    }
    let mut replacements = Replacements::new();
    for (uri, document) in index {
        for occurrence in &document.occurrences {
            if resolve_occurrence(index, uri, occurrence, false).as_ref() == Some(symbol) {
                replacements
                    .entry(uri.clone())
                    .or_default()
                    .push((occurrence.span.start(), occurrence.span.end()));
            }
        }
    }
    let mut renamed = Index::new();
    let mut changes = std::collections::HashMap::new();
    for (uri, document) in index {
        let mut source = document.source.clone();
        if let Some(spans) = replacements.get_mut(uri) {
            spans.sort_unstable();
            spans.dedup();
            let mut edits = Vec::new();
            for &(start, end) in spans.iter().rev() {
                if source.get(start..end) != Some(old_name) {
                    return Err(Error::invalid_params("This binding cannot be renamed safely"));
                }
                edits.push(TextEdit {
                    range: Range::new(
                        kcl_lib::byte_to_lsp_position(&document.source, start).ok_or_else(Error::content_modified)?,
                        kcl_lib::byte_to_lsp_position(&document.source, end).ok_or_else(Error::content_modified)?,
                    ),
                    new_text: name.to_owned(),
                });
                source.replace_range(start..end, name);
            }
            edits.reverse();
            changes.insert(uri.clone(), edits);
        }
        let document = Document::parse(uri, source)
            .ok_or_else(|| Error::invalid_params("The rename would produce invalid KCL"))?;
        renamed.insert(uri.clone(), document);
    }
    // Compare every binding before and after the proposed edit. This catches
    // duplicate declarations, captured references and newly captured names.
    for (uri, document) in index {
        for occurrence in &document.occurrences {
            let expected = resolve_occurrence(index, uri, occurrence, false)
                .map(|symbol| renamed_symbol(&replacements, symbol, name));
            let offset = renamed_offset(&replacements, uri, occurrence.span.start(), name);
            let next = renamed[uri].occurrences.iter().find(|item| item.span.start() == offset);
            let actual = next.and_then(|item| resolve_occurrence(&renamed, uri, item, false));
            if expected != actual {
                return Err(Error::invalid_params(
                    "The rename would collide with or capture another binding",
                ));
            }
        }
    }
    Ok(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

impl Backend {
    pub(super) async fn lexical_prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Option<PrepareRenameResponse> {
        let (index, _) = self
            .navigation_index_with_errors(&params.text_document.uri, false)
            .await;
        let occurrence = occurrence_at(&index, &params)?;
        let symbol = resolve_occurrence(&index, &params.text_document.uri, occurrence, false)?;
        let document = index.get(&symbol.uri)?;
        let binding = document
            .bindings
            .iter()
            .find(|binding| binding.span.start() == symbol.start && binding.span.end() == symbol.end)?;
        // Implicit module names have no identifier span to replace. File rename
        // and alias insertion are separate operations.
        if document.source.get(symbol.start..symbol.end)? != binding.name {
            return None;
        }
        let source = &index.get(&params.text_document.uri)?.source;
        Some(PrepareRenameResponse::Range(Range::new(
            kcl_lib::byte_to_lsp_position(source, occurrence.span.start())?,
            kcl_lib::byte_to_lsp_position(source, occurrence.span.end())?,
        )))
    }

    pub(super) async fn lexical_rename(
        &self,
        params: TextDocumentPositionParams,
        name: &str,
    ) -> Result<Option<WorkspaceEdit>> {
        let versions: BTreeMap<_, _> = self
            .open_documents
            .iter()
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect();
        let (index, failed) = self.navigation_index_with_errors(&params.text_document.uri, true).await;
        if !failed.is_empty() {
            return Err(Error::invalid_params(format!(
                "Cannot safely rename: could not read or parse {}",
                failed[0]
            )));
        }
        let Some(occurrence) = occurrence_at(&index, &params) else {
            return Ok(None);
        };
        let Some(symbol) = resolve_occurrence(&index, &params.text_document.uri, occurrence, false) else {
            return Ok(None);
        };
        if self.lexical_prepare_rename(params).await.is_none() {
            return Ok(None);
        }
        if index[&symbol.uri].source.get(symbol.start..symbol.end) == Some(name) {
            return Ok(None);
        }
        let mut edit = rename_edits(&index, &symbol, name)?;
        for (uri, document) in &index {
            if self.open_documents.contains_key(uri.as_str()) {
                if !self
                    .code_map
                    .get(uri.as_str())
                    .is_some_and(|contents| contents.as_slice() == document.source.as_bytes())
                {
                    return Err(Error::content_modified());
                }
            } else {
                let path = super::navigation::document_path(uri).ok_or_else(Error::content_modified)?;
                if self.fs.read_to_string(&path, Default::default()).await.ok().as_deref()
                    != Some(document.source.as_str())
                {
                    return Err(Error::content_modified());
                }
            }
        }
        if versions
            != self
                .open_documents
                .iter()
                .map(|entry| (entry.key().clone(), *entry.value()))
                .collect()
        {
            return Err(Error::content_modified());
        }
        if self.supports_document_changes.load(Ordering::Relaxed) {
            let mut edits: Vec<_> = edit
                .changes
                .take()
                .unwrap_or_default()
                .into_iter()
                .map(|(uri, edits)| TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        version: versions.get(uri.as_str()).copied(),
                        uri,
                    },
                    edits: edits.into_iter().map(OneOf::Left).collect(),
                })
                .collect();
            edits.sort_by(|a, b| a.text_document.uri.as_str().cmp(b.text_document.uri.as_str()));
            edit.document_changes = Some(DocumentChanges::Edits(edits));
        }
        Ok(Some(edit))
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::LanguageServer;
    use tower_lsp::lsp_types::*;

    use super::*;

    fn document(index: &mut Index, file: &str, source: &str) -> Url {
        let uri = Url::parse(&format!("file:///project/{file}")).unwrap();
        let source = format!("@settings(kclVersion = 3.0)\n{source}");
        index.insert(uri.clone(), Document::parse(&uri, source).unwrap());
        uri
    }

    fn params(index: &Index, uri: &Url, needle: &str) -> TextDocumentPositionParams {
        let source = &index[uri].source;
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: kcl_lib::byte_to_lsp_position(source, source.find(needle).unwrap()).unwrap(),
        }
    }

    fn rename(index: &Index, uri: &Url, needle: &str, name: &str) -> Result<WorkspaceEdit> {
        let at = params(index, uri, needle);
        let occurrence = occurrence_at(index, &at).unwrap();
        let symbol = resolve_occurrence(index, uri, occurrence, false).unwrap();
        rename_edits(index, &symbol, name)
    }

    fn edited(index: &Index, edit: &WorkspaceEdit, uri: &Url) -> String {
        let source = &index[uri].source;
        let mut result = source.clone();
        if let Some(edits) = edit.changes.as_ref().unwrap().get(uri) {
            for edit in edits.iter().rev() {
                let start = kcl_lib::lsp_position_to_byte(source, edit.range.start).unwrap();
                let end = kcl_lib::lsp_position_to_byte(source, edit.range.end).unwrap();
                result.replace_range(start..end, &edit.new_text);
            }
        }
        result
    }

    #[test]
    fn workspace_rename_preserves_aliases_comments_and_formatting() {
        let mut index = Index::new();
        let dims = document(&mut index, "dims.kcl", "export width=20mm  // width stays\n");
        let main = document(
            &mut index,
            "main.kcl",
            "import width as w from \"dims.kcl\"\nvalue=w\nlabel=\"width\"\n",
        );
        let direct = document(&mut index, "direct.kcl", "import width from \"dims.kcl\"\nx=width\n");
        let member = document(&mut index, "member.kcl", "import \"dims.kcl\" as d\nx=d.width\n");
        let public = document(
            &mut index,
            "public.kcl",
            "export import width as publicWidth from \"dims.kcl\"\n",
        );
        let consumer = document(
            &mut index,
            "consumer.kcl",
            "import publicWidth from \"public.kcl\"\nx=publicWidth\n",
        );
        let edit = rename(&index, &dims, "width=", "panelWidth").unwrap();
        assert!(edited(&index, &edit, &dims).ends_with("export panelWidth=20mm  // width stays\n"));
        assert!(
            edited(&index, &edit, &main)
                .ends_with("import panelWidth as w from \"dims.kcl\"\nvalue=w\nlabel=\"width\"\n")
        );
        assert!(edited(&index, &edit, &direct).ends_with("import panelWidth from \"dims.kcl\"\nx=panelWidth\n"));
        assert!(edited(&index, &edit, &member).ends_with("import \"dims.kcl\" as d\nx=d.panelWidth\n"));
        assert!(
            edited(&index, &edit, &public).ends_with("export import panelWidth as publicWidth from \"dims.kcl\"\n")
        );
        assert_eq!(edited(&index, &edit, &consumer), index[&consumer].source);
    }

    #[test]
    fn workspace_rename_aliases_and_reexports_have_their_own_binding() {
        let mut index = Index::new();
        let dims = document(&mut index, "dims.kcl", "export width=20mm\n");
        let main = document(
            &mut index,
            "main.kcl",
            "export import width as w from \"dims.kcl\"\nx=w\n",
        );
        let use_alias = document(
            &mut index,
            "consumer.kcl",
            "import w as local from \"main.kcl\"\nx=local\n",
        );
        let edit = rename(&index, &main, "w\n", "span").unwrap();
        assert_eq!(edited(&index, &edit, &dims), index[&dims].source);
        assert!(edited(&index, &edit, &main).ends_with("export import width as span from \"dims.kcl\"\nx=span\n"));
        assert!(edited(&index, &edit, &use_alias).ends_with("import span as local from \"main.kcl\"\nx=local\n"));
    }

    #[test]
    fn workspace_rename_respects_shadowing_and_rejects_capture() {
        let mut index = Index::new();
        let main = document(
            &mut index,
            "main.kcl",
            "value=1\nfn f(@value) { return value + 2 }\nx=value\nobj={ value = value }\n",
        );
        let edit = rename(&index, &main, "value=", "total").unwrap();
        assert!(
            edited(&index, &edit, &main)
                .ends_with("total=1\nfn f(@value) { return value + 2 }\nx=total\nobj={ value = total }\n")
        );
        document(&mut index, "main.kcl", "value=1\nother=2\nx=value+other\n");
        rename(&index, &main, "value=", "other").unwrap_err();
        document(
            &mut index,
            "main.kcl",
            "value=1\nfn f(@other) { return value + other }\n",
        );
        rename(&index, &main, "value=", "other").unwrap_err();
        rename(&index, &main, "other)", "value").unwrap_err();
        rename(&index, &main, "value=", "two words").unwrap_err();
        rename(&index, &main, "value=", "if").unwrap_err();
    }

    #[test]
    fn workspace_rename_parameters_update_resolved_keyword_arguments() {
        let mut index = Index::new();
        let dimensions = document(&mut index, "dims.kcl", "export fn size(width) { return width * 2 }\n");
        let main = document(
            &mut index,
            "main.kcl",
            "import size as measure from \"dims.kcl\"\nvalue = measure(width = 2mm)\n",
        );
        let edit = rename(&index, &dimensions, "width)", "span").unwrap();
        assert!(edited(&index, &edit, &dimensions).ends_with("export fn size(span) { return span * 2 }\n"));
        assert!(edited(&index, &edit, &main).ends_with("value = measure(span = 2mm)\n"));
    }

    #[test]
    fn workspace_rename_namespace_globs_and_unicode_ranges() {
        let mut index = Index::new();
        let dims = document(&mut index, "dims.kcl", "export width=20mm\n");
        let namespace = document(&mut index, "namespace.kcl", "import \"dims.kcl\" as d\nx=d.width\n");
        let edit = rename(&index, &namespace, "d.width", "sizes").unwrap();
        assert!(edited(&index, &edit, &namespace).ends_with("import \"dims.kcl\" as sizes\nx=sizes.width\n"));
        assert_eq!(edited(&index, &edit, &dims), index[&dims].source);
        let glob = document(
            &mut index,
            "glob.kcl",
            "import * from \"dims.kcl\"\nvalues=[\"😀\", width]\n",
        );
        let edit = rename(&index, &dims, "width=", "span").unwrap();
        assert!(edited(&index, &edit, &glob).ends_with("import * from \"dims.kcl\"\nvalues=[\"😀\", span]\n"));
    }

    #[test]
    fn workspace_rename_types_and_nested_module_aliases() {
        let mut index = Index::new();
        let dims = document(&mut index, "dims.kcl", "export type Distance = number(mm)\n");
        let main = document(
            &mut index,
            "main.kcl",
            "import Distance from \"dims.kcl\"\ntype Record = { Distance: Distance }\nfn identity(@value: Distance): Distance { return value }\nx = (2mm: Distance)\n",
        );
        let edit = rename(&index, &dims, "Distance =", "Length").unwrap();
        assert!(edited(&index, &edit, &main).ends_with("import Length from \"dims.kcl\"\ntype Record = { Distance: Length }\nfn identity(@value: Length): Length { return value }\nx = (2mm: Length)\n"));
        let public = document(&mut index, "public.kcl", "export import \"dims.kcl\" as dims\n");
        let nested = document(
            &mut index,
            "nested.kcl",
            "import \"public.kcl\" as lib\nfn identity(@value: lib::dims::Distance): lib::dims::Distance { return value }\n",
        );
        let edit = rename(&index, &public, "dims\n", "measurements").unwrap();
        assert!(edited(&index, &edit, &nested).ends_with(
            "fn identity(@value: lib::measurements::Distance): lib::measurements::Distance { return value }\n"
        ));
    }

    #[test]
    fn workspace_rename_refuses_ambiguous_parameter_type_names() {
        let mut index = Index::new();
        let uri = document(
            &mut index,
            "main.kcl",
            "type Distance = number(mm)\nfn identity(@Distance: Distance) { return Distance }\n",
        );
        rename(&index, &uri, "Distance =", "Length").unwrap_err();
        rename(&index, &uri, "Distance:", "value").unwrap_err();
    }

    #[tokio::test]
    async fn workspace_rename_versions_unsaved_documents_and_keeps_disk_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let dims = dir.path().join("dims.kcl");
        let main = dir.path().join("main.kcl");
        let header = "@settings(kclVersion = 3.0)\n";
        std::fs::write(&dims, format!("{header}export width=20mm\n")).unwrap();
        let disk = format!("{header}import width from \"dims.kcl\"\nx=width\n");
        std::fs::write(&main, &disk).unwrap();
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        server
            .initialize(InitializeParams {
                capabilities: ClientCapabilities {
                    workspace: Some(WorkspaceClientCapabilities {
                        workspace_edit: Some(WorkspaceEditClientCapabilities {
                            document_changes: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
        let uri = Url::from_file_path(&main).unwrap();
        let buffer = format!("{disk}y=width\n");
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "kcl".into(),
                    version: 7,
                    text: buffer,
                },
            })
            .await;
        let edit = server
            .lexical_rename(
                TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: Position::new(2, 2),
                },
                "panelWidth",
            )
            .await
            .unwrap()
            .unwrap();
        let Some(DocumentChanges::Edits(edits)) = edit.document_changes else {
            panic!("expected versioned edits");
        };
        let open = edits.iter().find(|edit| edit.text_document.uri == uri).unwrap();
        assert_eq!(open.text_document.version, Some(7));
        assert_eq!(open.edits.len(), 3);
        assert_eq!(
            edits
                .iter()
                .find(|edit| edit.text_document.uri == Url::from_file_path(&dims).unwrap())
                .unwrap()
                .text_document
                .version,
            None
        );
        assert_eq!(std::fs::read_to_string(&main).unwrap(), disk);
        std::fs::write(dir.path().join("broken.kcl"), "value =").unwrap();
        server
            .lexical_rename(
                TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position::new(2, 2),
                },
                "panelWidth",
            )
            .await
            .unwrap_err();
    }
}
