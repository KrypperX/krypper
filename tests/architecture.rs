use std::fs;
use std::path::Path;

#[path = "architecture/guard.rs"]
mod guard;

use guard::{
    scan_crate_root_aliases, scan_module_file, scan_source, scan_source_at_depth,
    scan_source_with_root_exports,
};

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

    let raw_identifier_violations =
        scan_source("use crate::r#brain::Decision;", "raw_brain_identifier.rs");
    assert_eq!(
        raw_identifier_violations.len(),
        1,
        "{raw_identifier_violations:?}"
    );
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

    let self_crate_alias_violations = scan_source(
        "extern crate self as root; use root::brain::Decision;",
        "self_crate_alias.rs",
    );
    assert!(
        self_crate_alias_violations
            .iter()
            .any(|violation| violation.contains("self crate alias")),
        "{self_crate_alias_violations:?}"
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

    let hidden_raw_path = "macro_rules! hidden { () => { use crate::r#brain::Decision; } }";
    let hidden_raw_path_violations = scan_source(hidden_raw_path, "macro_raw_path.rs");
    assert!(
        hidden_raw_path_violations
            .iter()
            .any(|violation| violation.contains("macro tokens")),
        "{hidden_raw_path_violations:?}"
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

    let custom_use_attribute = scan_source(
        "#[custom_codegen] use std::fmt::Debug;",
        "custom_use_attribute.rs",
    );
    assert!(
        custom_use_attribute
            .iter()
            .any(|violation| violation.contains("uninspected production attribute")),
        "{custom_use_attribute:?}"
    );

    let module_attribute = scan_source("#[custom_codegen] mod nested {}", "module_attribute.rs");
    assert!(
        module_attribute
            .iter()
            .any(|violation| violation.contains("uninspected production module attribute")),
        "{module_attribute:?}"
    );

    let conditional_module_path = scan_source(
        "#[cfg_attr(unix, path = \"selected.rs\")] mod nested;",
        "conditional_module_path.rs",
    );
    assert!(
        conditional_module_path
            .iter()
            .any(|violation| violation.contains("uninspected production module attribute")),
        "{conditional_module_path:?}"
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

    let shadowed_builtin_derive = scan_source(
        "use untrusted::Clone; #[derive(Clone)] struct Item;",
        "shadowed_builtin_derive.rs",
    );
    assert!(
        shadowed_builtin_derive
            .iter()
            .any(|violation| violation.contains("macro import")),
        "{shadowed_builtin_derive:?}"
    );

    let inspected_builtin_derive_import = scan_source(
        "use std::clone::Clone; #[derive(Clone)] struct Item;",
        "inspected_builtin_derive_import.rs",
    );
    assert!(
        inspected_builtin_derive_import.is_empty(),
        "{inspected_builtin_derive_import:?}"
    );

    let inspected_derive = scan_source(
        "use serde::Serialize; #[derive(Serialize)] struct Item;",
        "inspected_derive.rs",
    );
    assert!(inspected_derive.is_empty(), "{inspected_derive:?}");

    let serde_path = scan_source(
        r#"use serde::Serialize;
            #[derive(Serialize)]
            struct Item { #[serde(serialize_with = "crate::brain::serialize")] value: String }
        "#,
        "serde_path.rs",
    );
    assert!(
        serde_path
            .iter()
            .any(|violation| violation.contains("uninspected production attribute `serde`")),
        "{serde_path:?}"
    );

    let serde_relative_path = scan_source(
        r#"use serde::Serialize;
            #[derive(Serialize)]
            struct Item { #[serde(with = "super::brain::codec")] value: String }
        "#,
        "serde_relative_path.rs",
    );
    assert!(
        serde_relative_path
            .iter()
            .any(|violation| violation.contains("uninspected production attribute `serde`")),
        "{serde_relative_path:?}"
    );

    let serde_rename = scan_source(
        r#"use serde::Serialize;
            #[derive(Serialize)]
            struct Item { #[serde(rename = "crate::brain")] value: String }
        "#,
        "serde_rename.rs",
    );
    assert!(serde_rename.is_empty(), "{serde_rename:?}");

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
fn direction_guard_ignores_test_only_fields_and_variants() {
    let source = r#"
        struct Widget {
            #[cfg(test)]
            test_only: crate::brain::TestOnly,
        }
        enum Event {
            #[cfg(all(test, unix))]
            TestOnly(crate::brain::TestOnly),
        }
    "#;

    let violations = scan_source(source, "test_only_fields.rs");

    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn direction_guard_still_checks_production_fields_and_variants() {
    let source = r#"
        struct Widget { production: crate::brain::Production }
        enum Event { Production(crate::brain::Production) }
    "#;

    let violations = scan_source(source, "production_fields.rs");

    assert_eq!(violations.len(), 2, "{violations:?}");
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

#[test]
fn crate_root_brain_reexports_and_globs_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");

    for (root_source, exported_name) in [
        ("pub(crate) use brain as brain_api;", "brain_api"),
        (
            "pub(crate) use crate::r#brain::Decision as PublicDecision;",
            "PublicDecision",
        ),
        (
            "pub(crate) type BrainDecision = crate::brain::Decision;",
            "BrainDecision",
        ),
    ] {
        fs::write(&root, root_source).unwrap();
        let mut violations = Vec::new();

        let brain_exports = scan_crate_root_aliases(&root, &mut violations);

        assert!(
            brain_exports.contains(exported_name),
            "source `{root_source}` did not record its Brain export: {brain_exports:?}"
        );

        let tool_source = format!("use crate::{exported_name};");
        let tool_violations =
            scan_source_with_root_exports(&tool_source, "tools_alias.rs", 1, &brain_exports);
        assert!(
            !tool_violations.is_empty(),
            "Tools import through `{exported_name}` was not rejected"
        );
    }

    fs::write(
        &root,
        "type BrainDecision = crate::brain::Decision; type Alias = BrainDecision;",
    )
    .unwrap();
    let mut violations = Vec::new();
    let brain_exports = scan_crate_root_aliases(&root, &mut violations);
    assert!(brain_exports.contains("Alias"), "{brain_exports:?}");
    let alias_chain_violations = scan_source_with_root_exports(
        "use crate::Alias;",
        "tools_alias_chain.rs",
        1,
        &brain_exports,
    );
    assert!(
        !alias_chain_violations.is_empty(),
        "a root type-alias chain to Brain was not rejected"
    );

    fs::write(&root, "pub(crate) use brain::*;").unwrap();
    let mut violations = Vec::new();
    let _ = scan_crate_root_aliases(&root, &mut violations);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Brain glob import")),
        "Brain glob should fail closed: {violations:?}"
    );
}

#[test]
fn crate_root_inline_facades_and_transitive_reexports_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(
        &root,
        r#"
            pub mod compatibility {
                pub use crate::extensions::provider::PublicModelError as ModelError;
            }
            pub use crate::compatibility::ModelError as PublicModelError;
            pub mod extensions {
                pub mod provider {
                    pub use crate::brain::ModelError as PublicModelError;
                }
            }
            pub mod type_aliases {
                pub(crate) type DecisionAlias = crate::brain::Decision;
            }
        "#,
    )
    .unwrap();
    let mut violations = Vec::new();

    let brain_exports = scan_crate_root_aliases(&root, &mut violations);

    for (facade, item) in [
        ("compatibility", "ModelError"),
        ("extensions", "provider::PublicModelError"),
        ("type_aliases", "DecisionAlias"),
    ] {
        assert!(
            brain_exports.contains(facade),
            "facade `{facade}` was not recorded: {brain_exports:?}"
        );
        let tool_source = format!("use crate::{facade}::{item};");
        let tool_violations =
            scan_source_with_root_exports(&tool_source, "tools_facade.rs", 1, &brain_exports);
        assert!(
            !tool_violations.is_empty(),
            "Tools import through `{facade}::{item}` was not rejected"
        );
    }
    assert!(brain_exports.contains("PublicModelError"));
    let root_alias_violations = scan_source_with_root_exports(
        "use crate::PublicModelError;",
        "tools_root_facade_alias.rs",
        1,
        &brain_exports,
    );
    assert!(
        !root_alias_violations.is_empty(),
        "a crate-root re-export of an inline facade was not rejected"
    );
}

#[test]
fn crate_root_external_facades_that_reexport_brain_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(&root, "pub mod facade;").unwrap();
    fs::write(
        directory.path().join("facade.rs"),
        "pub use crate::brain::ModelError;",
    )
    .unwrap();
    let mut violations = Vec::new();

    let brain_exports = scan_crate_root_aliases(&root, &mut violations);

    assert!(
        brain_exports.contains("facade"),
        "external Brain facade was not recorded: {brain_exports:?}"
    );
    let tool_violations = scan_source_with_root_exports(
        "use crate::facade::ModelError;",
        "tools_external_facade.rs",
        1,
        &brain_exports,
    );
    assert!(
        !tool_violations.is_empty(),
        "Tools import through an external Brain facade was not rejected"
    );
}

#[test]
fn module_scanner_resolves_inline_path_attributes_and_reports_read_failures() {
    let directory = tempfile::tempdir().unwrap();
    let source_directory = directory.path().join("src");
    let inline_directory = source_directory.join("tools/inline");
    fs::create_dir_all(&inline_directory).unwrap();
    fs::write(source_directory.join("lib.rs"), "mod tools;").unwrap();

    let tools_root = source_directory.join("tools.rs");
    fs::write(
        &tools_root,
        r#"
            mod inline {
                #[path = "child.rs"]
                mod child;
            }
        "#,
    )
    .unwrap();
    fs::write(
        inline_directory.join("child.rs"),
        "use crate::brain::HiddenDependency;",
    )
    .unwrap();

    let violations = scan_module_file(&tools_root);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("crate::brain")),
        "inline module path target was not scanned: {violations:?}"
    );

    fs::write(&tools_root, "#[path = \"missing.rs\"] mod missing;").unwrap();
    let violations = scan_module_file(&tools_root);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("unable to read Rust source")),
        "an unreadable path target did not fail closed: {violations:?}"
    );
}
