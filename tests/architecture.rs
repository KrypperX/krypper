use std::fs;
use std::path::Path;

#[path = "architecture/guard.rs"]
mod guard;

use guard::{scan_crate_root_aliases, scan_module_file, scan_source, scan_source_at_depth};

#[test]
fn production_tools_modules_do_not_import_brain() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools.rs");
    let violations = scan_module_file(&root);

    assert!(
        violations.is_empty(),
        "production Tools modules must not depend on Brain:\n{}",
        violations.join("\n")
    );
}

#[test]
fn direction_guard_detects_grouped_imports_and_ignores_test_only_items() {
    let source = r#"
        use crate::{brain::ProductionDecision, tools::Registry};

        #[cfg(all(test, unix))]
        mod tests {
            use crate::brain::TestOnlyHelper;
        }
    "#;
    let violations = scan_source(source, "mixed.rs");

    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(violations[0].contains("mixed.rs"));
}

#[test]
fn direction_guard_detects_qualified_paths() {
    let source = "fn access() { let _: crate::brain::Decision; }";

    let violations = scan_source(source, "qualified.rs");

    assert_eq!(violations.len(), 1, "{violations:?}");
}

#[test]
fn direction_guard_detects_relative_paths_crate_aliases_and_root_globs() {
    let root_path = "use super::brain::Decision;";
    let root_violations = scan_source(root_path, "relative_root.rs");
    assert_eq!(root_violations.len(), 1, "{root_violations:?}");

    let nested_path = r#"
        mod nested {
            fn access() { let _: self::super::super::brain::Decision; }
        }
    "#;
    let nested_violations = scan_source(nested_path, "relative_nested.rs");
    assert_eq!(nested_violations.len(), 1, "{nested_violations:?}");

    let direct_nested = "use super::super::brain::Decision;";
    let direct_nested_violations = scan_source_at_depth(direct_nested, "nested.rs", 2);
    assert_eq!(
        direct_nested_violations.len(),
        1,
        "{direct_nested_violations:?}"
    );

    let alias = "use crate as root; use root::brain::Decision;";
    let alias_violations = scan_source(alias, "crate_alias.rs");
    assert!(
        alias_violations
            .iter()
            .any(|violation| violation.contains("alias")),
        "{alias_violations:?}"
    );

    let grouped_alias = "use crate::{self as root}; use root::brain::Decision;";
    let grouped_alias_violations = scan_source(grouped_alias, "grouped_crate_alias.rs");
    assert!(
        grouped_alias_violations
            .iter()
            .any(|violation| violation.contains("alias")),
        "{grouped_alias_violations:?}"
    );

    let glob = "use super::*;";
    let glob_violations = scan_source(glob, "crate_root_glob.rs");
    assert!(
        glob_violations
            .iter()
            .any(|violation| violation.contains("glob")),
        "{glob_violations:?}"
    );
}

#[test]
fn direction_guard_inspects_macro_tokens_and_fails_closed_on_unknown_macros_and_includes() {
    let hidden_path = "macro_rules! hidden { () => { use crate::brain::Decision; } }";
    let macro_violations = scan_source(hidden_path, "macro_tokens.rs");
    assert!(
        macro_violations
            .iter()
            .any(|violation| violation.contains("macro tokens")),
        "{macro_violations:?}"
    );

    let hidden_alias = "external_macro!(use crate as root; root::brain::Decision);";
    let alias_violations = scan_source(hidden_alias, "macro_alias.rs");
    assert!(
        alias_violations
            .iter()
            .any(|violation| violation.contains("macro tokens")),
        "{alias_violations:?}"
    );

    let unknown_macro = scan_source("external_macro!();", "unknown_macro.rs");
    assert!(
        unknown_macro
            .iter()
            .any(|violation| violation.contains("uninspected production macro")),
        "{unknown_macro:?}"
    );

    let shadowed_macro = scan_source(
        "use untrusted::format; fn build() { format!(); }",
        "shadowed_macro.rs",
    );
    assert!(
        shadowed_macro
            .iter()
            .any(|violation| violation.contains("macro import")),
        "{shadowed_macro:?}"
    );

    let inspected_json_macro = scan_source(
        "use serde_json::json; fn build() { let _ = json!({\"ok\": true}); }",
        "inspected_json_macro.rs",
    );
    assert!(inspected_json_macro.is_empty(), "{inspected_json_macro:?}");

    let custom_attribute = scan_source("#[custom_codegen] struct Item;", "custom_attribute.rs");
    assert!(
        custom_attribute
            .iter()
            .any(|violation| violation.contains("uninspected production attribute")),
        "{custom_attribute:?}"
    );

    let shadowed_derive = scan_source(
        "use untrusted::Serialize; #[derive(Serialize)] struct Item;",
        "shadowed_derive.rs",
    );
    assert!(
        shadowed_derive
            .iter()
            .any(|violation| violation.contains("macro import")),
        "{shadowed_derive:?}"
    );

    let inspected_derive = scan_source(
        "use serde::Serialize; #[derive(Serialize)] struct Item;",
        "inspected_derive.rs",
    );
    assert!(inspected_derive.is_empty(), "{inspected_derive:?}");

    let include = scan_source("include!(\"generated.rs\");", "included_source.rs");
    assert!(
        include
            .iter()
            .any(|violation| violation.contains("include! source is not inspected")),
        "{include:?}"
    );
}

#[test]
fn direction_guard_ignores_test_only_associated_items() {
    let source = r#"
        struct Widget;
        impl Widget {
            #[cfg(test)]
            fn test_only() { let _: crate::brain::TestOnly; }
        }
        trait Fixture {
            #[cfg(all(test, unix))]
            fn test_only() { let _: super::brain::TestOnly; }
        }
    "#;

    let violations = scan_source(source, "associated_test_items.rs");

    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn crate_root_aliases_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(&root, "use crate as root;").unwrap();
    let mut violations = Vec::new();

    scan_crate_root_aliases(&root, &mut violations);

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("alias")),
        "{violations:?}"
    );
}
