# KCL

This contains the KCL standard library, executor and test suite.

For using it, see [the API docs](https://docs.rs/kcl-lib).

For developing it, see [CONTRIBUTING.md](contributing.md)

## Offline checking (native Rust)

`check::check_source(source, executor_settings).await` checks source text using
fresh mock execution, including an editor's unsaved entry point. Supply
`current_file` for the entry-point path and `project_directory` for imports; the
current file's parent is used when no project directory is supplied. Imported
files are read from disk.

`check::check_file(path).await` loads an entry point and searches its directory
and parents for the nearest optional `project.toml`, matching the CLI. Imports
still resolve from the entry point's directory. File and settings errors are
returned as Rust errors; KCL errors
and warnings are returned in `CheckOutcome::diagnostics`. Use `has_errors()` and
`has_warnings()` to implement your own exit-status policy. Source maps retain
imported file contents and paths for diagnostic rendering.

`CheckOutcome::execution` retains the `ExecOutcome` for project-specific checks
when execution completes, and `execution_error` retains partial results when
execution fails. These internal execution types are not a stable JSON contract.
Checking does not use a Zoo client or API token and does not verify authoritative
CAD geometry. Cabinet-specific validation and manufacturing output belong in
the consuming project.
