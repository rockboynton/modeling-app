//! Workspace declarations from project files and unsaved editor buffers.
use std::collections::BTreeMap;
use std::collections::HashSet;

use tower_lsp::lsp_types::*;

use super::Backend;

fn matches(name: &str, query: &str) -> bool {
    let folded = name.to_lowercase();
    let mut name = folded.chars();
    query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|wanted| name.any(|c| c == wanted))
}

impl Backend {
    pub(super) async fn search_workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        let mut seeds: Vec<Url> = self
            .workspace_folders
            .iter()
            .filter_map(|folder| {
                let mut base = folder.uri.as_str().trim_end_matches('/').to_owned();
                base.push_str("/__kcl_workspace_symbols__.kcl");
                Url::parse(&base).ok()
            })
            .collect();
        seeds.extend(
            self.open_documents
                .iter()
                .filter_map(|entry| Url::parse(entry.key()).ok()),
        );
        let mut roots = HashSet::new();
        let mut documents = BTreeMap::new();
        for seed in seeds {
            #[cfg(not(target_arch = "wasm32"))]
            if let Ok(path) = seed.to_file_path()
                && !roots.insert(self.document_project_root(&path))
            {
                continue;
            }
            #[cfg(target_arch = "wasm32")]
            if !roots.insert(seed.clone()) {
                continue;
            }
            let source =
                (!self.open_documents.contains_key(seed.as_str())).then(|| "@settings(kclVersion = 3.0)\n".into());
            let (index, _) = self.navigation_index_with_source(&seed, true, source).await;
            documents.extend(index);
        }
        let mut symbols = vec![];
        for (uri, document) in documents {
            for binding in &document.bindings {
                if binding.import.is_some()
                    || !matches(&binding.name, query)
                    || document
                        .functions
                        .iter()
                        .any(|(_, scope)| scope.start() <= binding.span.start() && binding.span.end() <= scope.end())
                {
                    continue;
                }
                let Some(start) = kcl_lib::byte_to_lsp_position(&document.source, binding.span.start()) else {
                    continue;
                };
                let Some(end) = kcl_lib::byte_to_lsp_position(&document.source, binding.span.end()) else {
                    continue;
                };
                let kind = if document.functions.iter().any(|(span, _)| *span == binding.span) {
                    SymbolKind::FUNCTION
                } else if document.types.contains(&binding.span) {
                    SymbolKind::CLASS
                } else {
                    SymbolKind::CONSTANT
                };
                #[allow(deprecated)]
                symbols.push(SymbolInformation {
                    name: binding.name.clone(),
                    kind,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: uri.clone(),
                        range: Range::new(start, end),
                    },
                    container_name: uri
                        .path_segments()
                        .and_then(|mut parts| parts.next_back())
                        .map(str::to_owned),
                });
            }
        }
        symbols.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.location.uri.cmp(&b.location.uri)));
        symbols
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::LanguageServer;

    use super::*;

    #[tokio::test]
    async fn workspace_symbols_find_unopened_files_and_prefer_unsaved_declarations() {
        let server = crate::lsp::test_util::kcl_lsp_server(false).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let uri = Url::from_file_path(dir.path().join("panels.kcl")).unwrap();
        std::fs::write(
            dir.path().join("panels.kcl"),
            "@settings(kclVersion = 3.0)\nfn panelWidth(@size) { local = size\nreturn local }\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/ignored.kcl"), "ignored = 1").unwrap();
        server.workspace_folders.insert(
            "project".into(),
            WorkspaceFolder {
                uri: Url::from_directory_path(dir.path()).unwrap(),
                name: "project".into(),
            },
        );
        let symbols = server.search_workspace_symbols("PW").await;
        assert_eq!(symbols.len(), 1, "{symbols:?}");
        assert_eq!(symbols[0].name, "panelWidth");
        assert_eq!(symbols[0].location.uri, uri);
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        assert_eq!(server.search_workspace_symbols("").await.len(), 1);
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "kcl".into(),
                    version: 1,
                    text: "@settings(kclVersion = 3.0)\nnewWidth = 2mm\n".into(),
                },
            })
            .await;
        assert!(server.search_workspace_symbols("panel").await.is_empty());
        assert_eq!(server.search_workspace_symbols("nw").await[0].name, "newWidth");
    }
}
