//! Offline KCL validation for native tools and editors.
//!
//! Checks parse and execute KCL with a fresh mock context, without a Zoo client
//! or API token. They do not establish authoritative CAD geometry. Callers own
//! presentation and exit-status policy; execution data remains available for
//! project-specific checks rather than being part of a diagnostics JSON format.

use std::path::Path;

use anyhow::Context;
use indexmap::IndexMap;
use serde::Serialize;

use crate::CompilationIssue;
use crate::ExecOutcome;
use crate::ExecutorContext;
use crate::ExecutorSettings;
use crate::KclError;
use crate::KclErrorWithOutputs;
use crate::MockConfig;
use crate::ModuleId;
use crate::Program;
use crate::ProjectConfiguration;
use crate::SourceRange;
use crate::TypedPath;
use crate::kcl_error::Severity;
use crate::modules::ModulePath;
use crate::modules::ModuleSource;

/// A parse/execution issue or an error that stopped checking.
///
/// Existing compiler types retain suggestions, tags, error kinds, and all
/// related source ranges. Module IDs refer to [`CheckOutcome::source_files`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "diagnostic", rename_all = "snake_case")]
pub enum CheckDiagnostic {
    /// A warning or recoverable error raised by the compiler.
    Issue(CompilationIssue),
    /// A lexical, parse, or execution error that stopped checking.
    Error(KclError),
}

impl CheckDiagnostic {
    /// The severity to use for display and exit-status policy.
    pub fn severity(&self) -> Severity {
        match self {
            Self::Issue(issue) => issue.severity,
            Self::Error(_) => Severity::Fatal,
        }
    }

    /// The compiler's message, including the error category for hard errors.
    pub fn message(&self) -> String {
        match self {
            Self::Issue(issue) => issue.message.clone(),
            Self::Error(error) => error.get_message(),
        }
    }

    /// All locations associated with this diagnostic, using byte offsets.
    pub fn source_ranges(&self) -> Vec<SourceRange> {
        match self {
            Self::Issue(issue) => vec![issue.source_range],
            Self::Error(error) => error.source_ranges(),
        }
    }
}

/// Diagnostics and optional mock execution data from one independent check.
#[derive(Debug)]
pub struct CheckOutcome {
    /// Parse issues followed by execution issues and any hard error.
    pub diagnostics: Vec<CheckDiagnostic>,
    /// Source contents and paths for resolving diagnostics in imported modules.
    /// The entry point is always module zero, including when parsing fails.
    pub source_files: IndexMap<ModuleId, ModuleSource>,
    /// Execution data when mock execution completed, even if it raised issues.
    /// This is not an authoritative geometry result or a stable JSON contract.
    pub execution: Option<ExecOutcome>,
    /// Partial execution data when a hard error stopped execution.
    pub execution_error: Option<KclErrorWithOutputs>,
}

impl CheckOutcome {
    /// Whether any diagnostic is an error, including a fatal error.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|diagnostic| diagnostic.severity().is_err())
    }

    /// Whether any diagnostic is a warning.
    pub fn has_warnings(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity().is_warning())
    }
}

/// Check source text, which may be an editor's unsaved entry-point buffer.
///
/// Pass the entry point in `settings.current_file` and the import root in
/// `settings.project_directory`. If only the current file is supplied, its
/// parent becomes the import root. Imports are read from disk; unsaved imported
/// buffers are not substituted. Language version and units follow the source's
/// KCL settings annotations. Each check ignores memory from previous runs.
pub async fn check_source(source: &str, mut settings: ExecutorSettings) -> CheckOutcome {
    if settings.project_directory.is_none() {
        settings.project_directory = settings.current_file.as_ref().and_then(TypedPath::parent);
    }
    let path = settings
        .current_file
        .as_ref()
        .map(|path| ModulePath::Local {
            value: path.clone(),
            original_import_path: None,
        })
        .unwrap_or(ModulePath::Main);
    let mut report = CheckOutcome {
        diagnostics: Vec::new(),
        source_files: IndexMap::from([(
            ModuleId::default(),
            ModuleSource {
                path,
                source: source.to_owned(),
            },
        )]),
        execution: None,
        execution_error: None,
    };
    let program = match Program::parse(source) {
        Ok((program, issues)) => {
            report
                .diagnostics
                .extend(issues.into_iter().map(CheckDiagnostic::Issue));
            program
        }
        Err(error) => {
            report.diagnostics.push(CheckDiagnostic::Error(error));
            return report;
        }
    };
    // Do not execute a partial AST after parsing reports an error.
    if report.has_errors() {
        return report;
    }
    let Some(program) = program else {
        return report;
    };

    let context = ExecutorContext::new_mock(Some(settings)).await;
    let result = context
        .run_mock(
            &program,
            &MockConfig {
                use_prev_memory: false,
                ..Default::default()
            },
        )
        .await;
    context.close().await;
    match result {
        Ok(outcome) => {
            report
                .diagnostics
                .extend(outcome.issues.iter().cloned().map(CheckDiagnostic::Issue));
            merge_sources(&mut report.source_files, &outcome.source_files);
            report.execution = Some(outcome);
        }
        Err(error) => {
            // Warnings raised before a hard error must not disappear.
            report
                .diagnostics
                .extend(error.non_fatal.iter().cloned().map(CheckDiagnostic::Issue));
            report.diagnostics.push(CheckDiagnostic::Error(error.error.clone()));
            merge_sources(&mut report.source_files, &error.source_files);
            report.execution_error = Some(error);
        }
    }
    report
}

fn merge_sources(target: &mut IndexMap<ModuleId, ModuleSource>, sources: &IndexMap<ModuleId, ModuleSource>) {
    // Keep the caller's actual entry-point path rather than the executor's
    // synthetic `Main` path. Imported sources retain their own module IDs.
    target.extend(
        sources
            .iter()
            .filter(|(id, _)| !id.is_top_level())
            .map(|(id, source)| (*id, source.clone())),
    );
}

/// Load and check an entry point with the nearest optional `project.toml`.
///
/// Paths are canonicalized without changing the process's working directory.
/// Missing settings use defaults; unreadable or invalid settings are errors.
/// Settings are searched from the entry point's directory through its parents,
/// matching the CLI. Imports still resolve from the entry point's directory.
/// I/O/settings failures are returned separately from KCL diagnostics. Editors
/// with their own project/settings resolution should use [`check_source`].
pub async fn check_file(path: impl AsRef<Path>) -> anyhow::Result<CheckOutcome> {
    let requested = path.as_ref();
    let path = std::fs::canonicalize(requested)
        .with_context(|| format!("Cannot resolve KCL entry point {}", requested.display()))?;
    let settings = settings_for_file(&path)?;
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("Cannot read KCL source {}", path.display()))?;
    Ok(check_source(&source, settings).await)
}

/// Resolve project settings for a native editor's entry point, including a file
/// whose unsaved contents have not been written to disk yet.
/// Imports resolve from the entry point's parent, while project.toml is searched
/// through its ancestors. The caller supplies source text separately.
pub fn settings_for_file(path: &Path) -> anyhow::Result<ExecutorSettings> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("Cannot resolve working directory")?
            .join(path)
    };
    let directory = path.parent().context("KCL entry point has no parent directory")?;
    let mut settings = ExecutorSettings::default();
    for ancestor in directory.ancestors() {
        let settings_path = ancestor.join("project.toml");
        match std::fs::read_to_string(&settings_path) {
            Ok(text) => {
                settings = ProjectConfiguration::parse_and_validate(&text)
                    .with_context(|| format!("Invalid project settings {}", settings_path.display()))?
                    .into();
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot read project settings {}", settings_path.display()));
            }
        }
    }
    settings.current_file = Some(TypedPath(path.to_path_buf()));
    settings.project_directory = Some(TypedPath(directory.to_path_buf()));
    Ok(settings)
}
