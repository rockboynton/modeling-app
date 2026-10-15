//! Native editor execution with document paths, project settings and unsaved imports.
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use kcl_lib::ExecutorContext;
use kcl_lib::KclError;
use kcl_lib::MockConfig;
use kcl_lib::TypedPath;
use kcl_lib::kcl_error::KclErrorDetails;
use tower_lsp::lsp_types::TextDocumentItem;
use tower_lsp::lsp_types::Url;

use super::Backend;
use super::issues_in_top_level_module;
use crate::CompilationIssue;
use crate::KclErrorWithOutputs;
use crate::Program;
use crate::SourceRange;
use crate::fs::FileSystem;
use crate::fs::FileSystemHandle;

pub(super) fn project_root(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(path);
    parent
        .ancestors()
        .find(|dir| dir.join("project.toml").exists())
        .unwrap_or(parent)
        .to_path_buf()
}

pub(super) fn normalize(path: &Path) -> PathBuf {
    // Normalization must also work for a newly created, unsaved file.
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

impl Backend {
    pub(super) fn document_project_root(&self, path: &Path) -> PathBuf {
        let root = project_root(path);
        if root.join("project.toml").exists() {
            return root;
        }
        self.workspace_folders
            .iter()
            .filter_map(|folder| folder.uri.to_file_path().ok())
            .filter(|folder| path.starts_with(folder))
            .max_by_key(|folder| folder.components().count())
            .unwrap_or(root)
    }

    pub(super) async fn execute_project_document(
        &self,
        ast: &Program,
        uri: &Url,
    ) -> Result<(Vec<CompilationIssue>, Vec<tower_lsp::lsp_types::InlayHint>), KclErrorWithOutputs> {
        let path = uri
            .to_file_path()
            .map_err(|_| input_error("Document URI is not a local file"))?;
        let settings = kcl_lib::check::settings_for_file(&path).map_err(|error| input_error(&format!("{error:#}")))?;
        let buffers = self
            .open_documents
            .iter()
            .filter_map(|entry| {
                let uri = Url::parse(entry.key()).ok()?;
                let path = normalize(&uri.to_file_path().ok()?);
                Some((path, self.code_map.get(entry.key())?.clone()))
            })
            .collect();
        let mut context = ExecutorContext::new_mock(Some(settings)).await;
        context.fs = crate::fs::new_file_system_handle(EditorFiles {
            buffers,
            disk: self.fs.clone(),
        });
        let result = context
            .run_mock(
                ast,
                &MockConfig {
                    use_prev_memory: false,
                    ..Default::default()
                },
            )
            .await;
        let hints = if result.is_ok() {
            super::hints::inferred_type_hints(ast, &context).await
        } else {
            vec![]
        };
        context.close().await;
        result.map(|outcome| (issues_in_top_level_module(outcome.issues), hints))
    }

    pub(super) async fn refresh_project_documents(&self, changed: &Url) {
        let Some(path) = changed.to_file_path().ok() else {
            return;
        };
        let settings_changed = path.file_name().is_some_and(|name| name == "project.toml");
        let mock = self
            .executor_ctx
            .read()
            .await
            .as_ref()
            .is_some_and(ExecutorContext::is_mock);
        if !mock {
            return;
        }
        // Snapshot before awaiting: never hold a DashMap guard while checking.
        let documents: Vec<_> = self
            .open_documents
            .iter()
            .filter_map(|entry| {
                let uri = Url::parse(entry.key()).ok()?;
                if uri == *changed {
                    return None;
                }
                let text = String::from_utf8(self.code_map.get(entry.key())?.clone()).ok()?;
                Some(TextDocumentItem {
                    uri,
                    text,
                    version: 0,
                    language_id: "kcl".into(),
                })
            })
            .collect();
        for document in documents {
            let affected = if settings_changed {
                document
                    .uri
                    .to_file_path()
                    .ok()
                    .is_some_and(|file| path.parent().is_some_and(|parent| file.starts_with(parent)))
            } else {
                self.document_imports(&document.uri, changed).await
            };
            if affected {
                crate::lsp::backend::Backend::inner_on_change(self, document, true).await;
            }
        }
    }
}

fn input_error(message: &str) -> KclErrorWithOutputs {
    KclErrorWithOutputs::no_outputs(KclError::new_io(KclErrorDetails::new(
        message.to_owned(),
        vec![SourceRange::default()],
    )))
}

struct EditorFiles {
    buffers: HashMap<PathBuf, Vec<u8>>,
    disk: FileSystemHandle,
}

#[async_trait::async_trait]
impl FileSystem for EditorFiles {
    async fn read(&self, path: &TypedPath, range: SourceRange) -> Result<Vec<u8>, KclError> {
        if let Some(contents) = self.buffers.get(&normalize(&path.0)) {
            return Ok(contents.clone());
        }
        self.disk.read(path, range).await
    }
    async fn read_to_string(&self, path: &TypedPath, range: SourceRange) -> Result<String, KclError> {
        if let Some(contents) = self.buffers.get(&normalize(&path.0)) {
            return String::from_utf8(contents.clone())
                .map_err(|error| KclError::new_io(KclErrorDetails::new(error.to_string(), vec![range])));
        }
        self.disk.read_to_string(path, range).await
    }
    async fn exists(&self, path: &TypedPath, range: SourceRange) -> Result<bool, KclError> {
        if self.buffers.contains_key(&normalize(&path.0)) {
            return Ok(true);
        }
        self.disk.exists(path, range).await
    }
    async fn get_all_files(&self, path: &TypedPath, range: SourceRange) -> Result<Vec<TypedPath>, KclError> {
        self.disk.get_all_files(path, range).await
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp::LanguageServer;
    use tower_lsp::lsp_types::*;

    use super::*;

    async fn open(server: &Backend, path: &Path, text: &str) -> Url {
        let uri = Url::from_file_path(path).unwrap();
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    text: text.into(),
                    version: 1,
                    language_id: "kcl".into(),
                },
            })
            .await;
        uri
    }

    #[tokio::test]
    async fn project_diagnostics_use_paths_and_unsaved_imports_then_revert_on_close() {
        let dir = tempfile::tempdir().unwrap();
        let imported = dir.path().join("dimensions.kcl");
        std::fs::write(&imported, "export value = 7\n").unwrap();
        let server = crate::lsp::test_util::kcl_lsp_server_mock_execution().await.unwrap();
        let entry = open(
            &server,
            &dir.path().join("unsaved main.kcl"),
            "import value from \"dimensions.kcl\"\nresult = value + 1\n",
        )
        .await;
        assert!(!server.diagnostics_map.get(entry.as_str()).is_some_and(|items| {
            items
                .iter()
                .any(|item| item.severity == Some(DiagnosticSeverity::ERROR))
        }));
        let imported_uri = open(&server, &imported, "export value = \"not a number\"\n").await;
        assert!(
            server
                .diagnostics_map
                .get(entry.as_str())
                .unwrap()
                .iter()
                .any(|item| item.severity == Some(DiagnosticSeverity::ERROR))
        );
        server
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: imported_uri },
            })
            .await;
        assert!(!server.diagnostics_map.get(entry.as_str()).is_some_and(|items| {
            items
                .iter()
                .any(|item| item.severity == Some(DiagnosticSeverity::ERROR))
        }));
    }

    #[tokio::test]
    async fn project_settings_are_reloaded_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let server = crate::lsp::test_util::kcl_lsp_server_mock_execution().await.unwrap();
        std::fs::write(dir.path().join("project.toml"), "[not valid").unwrap();
        let uri = open(&server, &dir.path().join("main.kcl"), "value = 7\n").await;
        assert!(
            server
                .diagnostics_map
                .get(uri.as_str())
                .unwrap()
                .iter()
                .any(|item| item.message.contains("Invalid project settings"))
        );
        server
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })
            .await;
        open(&server, &dir.path().join("main.kcl"), "value = 7\n").await;
        assert!(
            server
                .diagnostics_map
                .get(uri.as_str())
                .unwrap()
                .iter()
                .any(|item| item.message.contains("Invalid project settings"))
        );
        std::fs::remove_file(dir.path().join("project.toml")).unwrap();
        server
            .did_save(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                text: None,
            })
            .await;
        assert!(!server.diagnostics_map.get(uri.as_str()).is_some_and(|items| {
            items
                .iter()
                .any(|item| item.severity == Some(DiagnosticSeverity::ERROR))
        }));
    }
    #[tokio::test]
    async fn project_navigation_finds_disk_imports_and_unsaved_references() {
        let dir = tempfile::tempdir().unwrap();
        let dims = dir.path().join("dimensions with spaces.kcl");
        std::fs::write(&dims, "export width = 7\n").unwrap();
        let server = crate::lsp::test_util::kcl_lsp_server_mock_execution().await.unwrap();
        let source = "import width as w from \"dimensions with spaces.kcl\"\na = w\n";
        let uri = open(&server, &dir.path().join("main.kcl"), source).await;
        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: Position::new(1, 4),
        };
        let Some(GotoDefinitionResponse::Scalar(definition)) = server.navigation_definition(params.clone()).await
        else {
            panic!("missing imported definition");
        };
        assert_eq!(definition.uri, Url::from_file_path(&dims).unwrap());
        assert_eq!(definition.range.start, Position::new(0, 7));
        let refs = server.navigation_references(params.clone(), false).await.unwrap();
        assert_eq!(refs.len(), 3); // imported name, alias, and a = w
        let refs = server.navigation_references(params.clone(), true).await.unwrap();
        assert_eq!(refs.len(), 4); // plus the exported declaration on disk
        open(&server, &dims, "// unsaved header\nexport width = 8\n").await;
        let Some(GotoDefinitionResponse::Scalar(definition)) = server.navigation_definition(params).await else {
            panic!("missing unsaved definition");
        };
        assert_eq!(definition.range.start, Position::new(1, 7));
    }
}
