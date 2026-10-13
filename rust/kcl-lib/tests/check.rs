use std::fs;

use kcl_lib::ExecutorSettings;
use kcl_lib::ModuleId;
use kcl_lib::TypedPath;
use kcl_lib::check::CheckDiagnostic;
use kcl_lib::check::check_file;
use kcl_lib::check::check_source;
use kcl_lib::kcl_error::Tag;

#[tokio::test]
async fn parse_errors_do_not_execute_and_keep_entry_point_source() {
    let source = "broken = (\n";
    let report = check_source(
        source,
        ExecutorSettings {
            current_file: Some("/project/unsaved.kcl".into()),
            ..Default::default()
        },
    )
    .await;
    assert!(report.has_errors());
    assert!(report.execution.is_none());
    assert!(report.execution_error.is_none());
    let entry = &report.source_files[&ModuleId::default()];
    assert_eq!(entry.source, source);
    assert_eq!(entry.path.to_string(), "/project/unsaved.kcl");
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.message().contains("Unexpected end of file"))
    );
}

#[tokio::test]
async fn deprecation_is_a_warning_and_execution_data_is_available() {
    // Explicitly exercise the old API under KCL 2.
    let source = "@settings(kclVersion = 2.0)\np = startSketchOn(XY)\n";
    let report = check_source(source, Default::default()).await;
    assert!(!report.has_errors(), "{:#?}", report.diagnostics);
    assert!(report.has_warnings());
    assert!(report.execution.unwrap().variables.contains_key("p"));
    assert!(report.diagnostics.iter().any(|d| matches!(d,
        CheckDiagnostic::Issue(issue) if issue.tag == Tag::Deprecated
            && &source[issue.source_range.start()..issue.source_range.end()] == "startSketchOn(XY)"
    )));
    let json = serde_json::to_value(&report.diagnostics).unwrap();
    assert_eq!(json[0]["kind"], "issue");
}

#[tokio::test]
async fn warning_before_a_hard_type_error_is_preserved() {
    let report = check_source(
        "@settings(kclVersion = 2.0)\np = startSketchOn(XY)\ns = sketch(on = XY) {\n  bottom = line(start = [0, 0], end = [4, 0])\n  right = line(start = bottom.end, end = [4, 3])\n}\n",
        Default::default(),
    )
    .await;
    assert!(report.has_errors());
    assert!(report.has_warnings());
    assert!(report.execution.is_none());
    assert!(report.execution_error.is_some());
    let diagnostic = report
        .diagnostics
        .iter()
        .find(|d| d.message().contains("Point2d"))
        .unwrap_or_else(|| panic!("{:#?}", report.diagnostics));
    assert!(diagnostic.message().contains("Segment"));
    let range = diagnostic.source_ranges()[0];
    assert_eq!(
        &report.source_files[&range.module_id()].source[range.start()..range.end()],
        "bottom.end"
    );
}

#[tokio::test]
async fn completed_execution_can_still_report_errors() {
    let report = check_source(
        "@settings(kclVersion = 3.0)\nfn inc(@x, @(experimental = true) amount? = 1) {\n  return x + amount\n}\nanswer = inc(5, amount = 2)\n",
        Default::default(),
    )
    .await;
    assert!(report.has_errors(), "{:#?}", report.diagnostics);
    assert!(report.execution_error.is_none());
    assert!(report.execution.unwrap().variables.contains_key("answer"));
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.message().contains("Use of `inc(amount)` is experimental"))
    );
}

#[tokio::test]
async fn imports_resolve_relative_to_the_entry_point_and_keep_error_sources() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("sub")).unwrap();
    let module = directory.path().join("sub/main.kcl");
    let module_source = "@settings(kclVersion = 3.0)\nexport shape = sketch(on = XY) {\n  bottom = line(start = [0, 0], end = [4, 0])\n  right = line(start = bottom.end, end = [4, 3])\n}\n";
    fs::write(&module, module_source).unwrap();
    let entry = directory.path().join("main.kcl");
    // The entry point need not exist on disk: source text can be unsaved.
    let report = check_source(
        "@settings(kclVersion = 3.0)\nimport shape from \"sub/main.kcl\"\n",
        ExecutorSettings {
            current_file: Some(TypedPath(entry)),
            ..Default::default()
        },
    )
    .await;
    assert!(report.has_errors());
    let diagnostic = report
        .diagnostics
        .iter()
        .find(|d| d.message().contains("Point2d"))
        .unwrap_or_else(|| panic!("{:#?}", report.diagnostics));
    let range = diagnostic
        .source_ranges()
        .into_iter()
        .find(|r| !r.module_id().is_top_level())
        .unwrap();
    let source = &report.source_files[&range.module_id()];
    assert_eq!(source.source, module_source);
    assert_eq!(source.path.to_string(), module.to_string_lossy());
    assert_eq!(&source.source[range.start()..range.end()], "bottom.end");
}

#[tokio::test]
async fn checks_do_not_reuse_previous_bindings() {
    let first = check_source("@settings(kclVersion = 3.0)\nremembered = 42\n", Default::default()).await;
    assert!(!first.has_errors());
    let second = check_source("@settings(kclVersion = 3.0)\nanswer = remembered\n", Default::default()).await;
    assert!(second.has_errors());
    assert!(second.diagnostics.iter().any(|d| d.message().contains("remembered")));
    let third = check_source("@settings(kclVersion = 3.0)\nanswer = 7\n", Default::default()).await;
    assert!(!third.has_errors(), "{:#?}", third.diagnostics);
    assert!(!third.has_warnings());
    assert!(third.execution.unwrap().variables.contains_key("answer"));
}

#[tokio::test]
async fn nearest_settings_are_used_without_changing_the_import_root() {
    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let entry = nested.join("main.kcl");
    fs::write(
        &entry,
        "@settings(kclVersion = 3.0)\nimport answer from \"value.kcl\"\n",
    )
    .unwrap();
    fs::write(
        nested.join("value.kcl"),
        "@settings(kclVersion = 3.0)\nexport answer = 7\n",
    )
    .unwrap();
    fs::write(directory.path().join("project.toml"), "[invalid toml").unwrap();
    check_file(&entry).await.unwrap_err();
    // The nearest settings override the invalid parent; relative imports must
    // continue to use the entry point's directory, not the settings directory.
    fs::write(
        nested.join("project.toml"),
        "[settings.modeling]\nhighlight_edges = false\n",
    )
    .unwrap();
    assert!(!check_file(&entry).await.unwrap().has_errors());
    fs::write(
        directory.path().join("project.toml"),
        "[settings.modeling]\nhighlight_edges = false\n",
    )
    .unwrap();
    fs::remove_file(nested.join("project.toml")).unwrap();
    assert!(!check_file(&entry).await.unwrap().has_errors());
}

#[tokio::test]
async fn file_checks_load_settings_and_report_invalid_settings() {
    let directory = tempfile::tempdir().unwrap();
    let entry = directory.path().join("main.kcl");
    fs::write(&entry, "@settings(kclVersion = 3.0)\nanswer = 7\n").unwrap();
    assert!(!check_file(&entry).await.unwrap().has_errors());
    let settings = directory.path().join("project.toml");
    fs::write(&settings, "[settings.modeling]\nhighlight_edges = false\n").unwrap();
    assert!(!check_file(&entry).await.unwrap().has_errors());
    fs::write(&settings, "[invalid toml").unwrap();
    let error = check_file(&entry).await.unwrap_err();
    assert!(error.to_string().contains("project.toml"));
    check_file(directory.path().join("missing.kcl")).await.unwrap_err();
}
