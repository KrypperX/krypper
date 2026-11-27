use std::fs;
use std::path::Path;

#[path = "architecture/guard.rs"]
mod guard;

use guard::{
    scan_brain_context_source, scan_brain_harness_context_dependencies, scan_crate_root_aliases,
    scan_module_file, scan_source, scan_source_at_depth, scan_source_with_root_exports,
    scan_tools_module_from_crate_root,
};

#[test]
fn production_tools_modules_do_not_import_brain_or_core() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    let violations = scan_tools_module_from_crate_root(&root);

    assert!(
        violations.is_empty(),
        "production Tools modules must not depend on Brain or Core:\n{}",
        violations.join("\n")
    );
}

#[test]
fn tools_core_guard_detects_direct_alias_facade_and_parent_paths() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    let sources = [
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub(crate) mod tools {
                use crate::core::task::Task;
                fn reference() { let _: Task; let _: crate::core::task::Task; }
            }
        "#,
        r#"
            use crate::core as runtime;
            pub mod core { pub mod task { pub struct Task; } }
            pub(crate) mod tools { use crate::runtime::task::Task; }
        "#,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub mod task_lifecycle { pub use crate::core::task::Task; }
            pub(crate) mod tools { use crate::task_lifecycle::Task; }
        "#,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub(crate) mod tools {
                mod nested { use super::super::core::task::Task; }
            }
        "#,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub(crate) mod tools { use super::core::task::Task; }
        "#,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub mod task_lifecycle { pub use crate::core::task::Task; }
            pub(crate) mod tools { use super::task_lifecycle::Task; }
        "#,
        r#"
            use crate as application;
            pub mod core { pub mod task { pub struct Task; } }
            pub(crate) mod tools { use crate::application::core::task::Task; }
        "#,
        r#"
            extern crate self as application;
            pub mod core { pub mod task { pub struct Task; } }
            pub(crate) mod tools { use application::core::task::Task; }
        "#,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub mod compatibility { pub use crate::core::task::Task; }
            pub(crate) mod tools { use crate::compatibility::Task; }
        "#,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            #[opaque::facade]
            pub mod compatibility {}
            pub(crate) mod tools { use crate::compatibility::Task; }
        "#,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub mod compatibility { define_core_facade!(); }
            pub(crate) mod tools { use crate::compatibility::Task; }
        "#,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub mod engine_facade { pub use crate::core as runtime; }
            pub mod compatibility {
                pub use crate::engine_facade::runtime::task::Task;
            }
            pub(crate) mod tools { use crate::compatibility::Task; }
        "#,
    ];

    for source in sources {
        fs::write(&root, source).unwrap();
        let violations = scan_tools_module_from_crate_root(&root);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("Core")),
            "Core dependency was not detected: {violations:?}"
        );
    }

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(
        &root,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub mod compatibility;
            pub(crate) mod tools { use crate::compatibility::Task; }
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("compatibility.rs"),
        "pub use crate::core::task::Task;",
    )
    .unwrap();
    let violations = scan_tools_module_from_crate_root(&root);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Core")),
        "external Core facade was not detected: {violations:?}"
    );
}

#[test]
fn tools_core_guard_detects_glob_include_and_external_transitive_facades() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(
        &root,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub use crate::core::*;
            pub(crate) mod tools { use crate::task::Task; }
        "#,
    )
    .unwrap();
    let violations = scan_tools_module_from_crate_root(&root);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Core")),
        "Core glob re-export was not detected: {violations:?}"
    );

    fs::write(
        &root,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub mod compatibility { include!("compatibility.rs"); }
            pub(crate) mod tools { use crate::compatibility::Task; }
        "#,
    )
    .unwrap();
    fs::write(
        directory.path().join("compatibility.rs"),
        "pub use crate::core::task::Task;",
    )
    .unwrap();
    let violations = scan_tools_module_from_crate_root(&root);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Core")),
        "included Core facade was not detected: {violations:?}"
    );

    fs::write(&root, "pub mod core { pub struct Task; }\npub mod facade_a;\npub mod facade_b;\npub(crate) mod tools { use crate::facade_a::Task; }").unwrap();
    fs::write(
        directory.path().join("facade_a.rs"),
        "pub use crate::facade_b::Task;",
    )
    .unwrap();
    fs::write(
        directory.path().join("facade_b.rs"),
        "pub use crate::core::Task;",
    )
    .unwrap();
    let violations = scan_tools_module_from_crate_root(&root);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Core")),
        "multi-hop external Core facade was not detected: {violations:?}"
    );
}

#[test]
fn tools_core_guard_handles_self_aliases_without_treating_the_crate_as_core() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(
        &root,
        r#"
            extern crate self as application;
            pub mod contracts { pub struct ProcessOutcome; }
            pub mod core { pub struct Task; }
            pub(crate) mod tools { use application::contracts::ProcessOutcome; }
        "#,
    )
    .unwrap();
    let violations = scan_tools_module_from_crate_root(&root);
    assert!(
        violations
            .iter()
            .all(|violation| !violation.contains("Core")),
        "neutral contracts through a self alias were classified as Core: {violations:?}"
    );

    fs::write(
        &root,
        r#"
            extern crate self as application;
            pub mod core { pub struct Task; }
            pub(crate) mod tools { use application::core::Task; }
        "#,
    )
    .unwrap();
    let violations = scan_tools_module_from_crate_root(&root);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("Core")),
        "Core path through a self alias was not detected: {violations:?}"
    );
}

#[test]
fn tools_core_guard_ignores_test_only_statements() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(
        &root,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub(crate) mod tools {
                fn production_helper() {
                    #[cfg(test)]
                    let _: crate::core::task::Task;
                }
            }
        "#,
    )
    .unwrap();

    let violations = scan_tools_module_from_crate_root(&root);
    assert!(
        violations.is_empty(),
        "test-only statement was classified as a production Core dependency: {violations:?}"
    );
}

#[test]
fn tools_core_guard_ignores_test_only_dependencies() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(
        &root,
        r#"
            pub mod core { pub mod task { pub struct Task; } }
            pub(crate) mod tools {
                #[cfg(test)]
                use crate::core::task::Task;
            }
        "#,
    )
    .unwrap();

    assert!(scan_tools_module_from_crate_root(&root).is_empty());
}

#[test]
fn production_brain_modules_do_not_import_harness_context() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/brain");
    let violations = scan_brain_harness_context_dependencies(&root);

    assert!(
        violations.is_empty(),
        "production Brain modules must use the shared context contract instead of Harness context:\n{}",
        violations.join("\n")
    );
}

#[test]
fn brain_context_guard_detects_grouped_and_qualified_paths_but_ignores_tests() {
    let source = r#"
        use crate::{harness::context::{ContextError, RepositoryContext}};
        mod nested {
            fn access() { let _: crate::harness::context::RepositorySnippet; }
        }
        macro_rules! hidden_dependency {
            () => { use crate::harness::context::ContextError; };
        }
        #[cfg(test)]
        mod tests {
            use crate::harness::context::ContextError;
        }
    "#;

    let violations = scan_brain_context_source(source, "brain_fixture.rs");

    assert_eq!(violations.len(), 4, "{violations:?}");
}

#[test]
fn brain_context_guard_detects_harness_alias_imports() {
    let source = r#"
        use crate::harness as h;
        use h::context::RepositoryContext;

        fn aliased_path() {
            let _: h::context::RepositorySnippet;
        }
    "#;

    let violations = scan_brain_context_source(source, "brain_alias_fixture.rs");

    assert_eq!(violations.len(), 2, "{violations:?}");
}

#[test]
fn brain_context_guard_resolves_parent_aliases_after_super() {
    let source = r#"
        use crate::harness as h;
        mod nested {
            use super::h::context::RepositoryContext;
        }
    "#;

    let violations = scan_brain_context_source(source, "brain_super_alias_fixture.rs");

    assert_eq!(violations.len(), 1, "{violations:?}");
}

#[test]
fn brain_context_guard_super_skips_block_import_scopes() {
    let source = r#"
        use crate::harness as h;
        mod nested {
            use crate::brain as h;
            fn access() {
                use super::h::context::RepositoryContext;
            }
        }
    "#;

    let violations = scan_brain_context_source(source, "brain_block_super_alias_fixture.rs");

    assert_eq!(violations.len(), 1, "{violations:?}");
}

#[test]
fn brain_context_guard_respects_alias_shadowing_between_modules() {
    let inline_source = r#"
        use crate::harness as h;
        mod nested {
            use crate::brain as h;
            use h::context::RepositoryContext;
        }
        mod sibling {
            use crate::brain as h;
            use h::context::RepositoryContext;
        }
    "#;
    let inline_violations =
        scan_brain_context_source(inline_source, "brain_shadowed_alias_fixture.rs");
    assert!(inline_violations.is_empty(), "{inline_violations:?}");

    let directory = tempfile::tempdir().unwrap();
    let brain_directory = directory.path().join("brain");
    fs::create_dir_all(&brain_directory).unwrap();
    fs::write(
        brain_directory.join("mod.rs"),
        "use crate::harness as h; mod nested;",
    )
    .unwrap();
    fs::write(
        brain_directory.join("nested.rs"),
        "use crate::brain as h; use h::context::RepositoryContext;",
    )
    .unwrap();
    let external_violations = scan_brain_harness_context_dependencies(&brain_directory);
    assert!(external_violations.is_empty(), "{external_violations:?}");
}

#[test]
fn brain_context_guard_revisits_shared_path_files_for_each_alias_context() {
    let directory = tempfile::tempdir().unwrap();
    let brain_directory = directory.path().join("brain");
    fs::create_dir_all(&brain_directory).unwrap();
    fs::write(
        brain_directory.join("mod.rs"),
        "mod safe; mod unsafe_module;",
    )
    .unwrap();
    fs::write(
        brain_directory.join("safe.rs"),
        "use crate::brain as h; #[path = \"shared.rs\"] mod shared;",
    )
    .unwrap();
    fs::write(
        brain_directory.join("unsafe_module.rs"),
        "use crate::harness as h; #[path = \"shared.rs\"] mod shared;",
    )
    .unwrap();
    fs::write(
        brain_directory.join("shared.rs"),
        "use super::h::context::RepositoryContext;",
    )
    .unwrap();

    let violations = scan_brain_harness_context_dependencies(&brain_directory);

    assert_eq!(violations.len(), 1, "{violations:?}");
}

#[test]
fn brain_context_guard_detects_self_alias_imports() {
    let source = r#"
        use crate::harness::{self as h};
        use h::context::RepositoryContext;
    "#;

    let violations = scan_brain_context_source(source, "brain_self_alias_fixture.rs");

    assert_eq!(violations.len(), 1, "{violations:?}");
}

#[test]
fn brain_context_guard_rejects_harness_globs() {
    let direct_glob = r#"
        use crate::harness::*;
        fn direct_glob_path() { let _: context::RepositoryContext; }
    "#;
    let alias_glob = r#"
        use crate::harness as h;
        use h::*;
        fn alias_glob_path() { let _: context::RepositoryContext; }
    "#;

    let direct_violations = scan_brain_context_source(direct_glob, "brain_direct_glob_fixture.rs");
    let alias_violations = scan_brain_context_source(alias_glob, "brain_alias_glob_fixture.rs");

    assert_eq!(direct_violations.len(), 1, "{direct_violations:?}");
    assert_eq!(alias_violations.len(), 1, "{alias_violations:?}");
}

#[test]
fn brain_context_guard_rejects_hidden_harness_imports_in_macros() {
    let source = r#"
        macro_rules! hidden_alias {
            () => {
                use crate::harness as h;
                use h::context::RepositoryContext;
            };
        }
        macro_rules! hidden_glob {
            () => { use crate::harness::*; };
        }
    "#;

    let violations = scan_brain_context_source(source, "brain_macro_fixture.rs");

    assert_eq!(violations.len(), 2, "{violations:?}");
}

#[test]
fn brain_context_guard_resolves_outer_harness_aliases_in_macros() {
    let source = r#"
        use crate::harness as h;
        macro_rules! hidden_alias {
            () => { use h::context::RepositoryContext; };
        }
        macro_rules! hidden_glob {
            () => { use h::*; };
        }
    "#;

    let violations = scan_brain_context_source(source, "brain_outer_alias_macro_fixture.rs");

    assert_eq!(violations.len(), 2, "{violations:?}");
}

#[test]
fn brain_context_guard_carries_parent_aliases_into_external_modules() {
    let directory = tempfile::tempdir().unwrap();
    let brain_directory = directory.path().join("brain");
    fs::create_dir_all(&brain_directory).unwrap();
    fs::write(
        brain_directory.join("mod.rs"),
        "use crate::harness as h; mod nested;",
    )
    .unwrap();
    fs::write(
        brain_directory.join("nested.rs"),
        "use super::h::context::RepositoryContext;",
    )
    .unwrap();

    let violations = scan_brain_harness_context_dependencies(&brain_directory);

    assert_eq!(violations.len(), 1, "{violations:?}");
}

#[test]
fn brain_context_guard_follows_external_path_attributes() {
    let directory = tempfile::tempdir().unwrap();
    let brain_directory = directory.path().join("src/brain");
    fs::create_dir_all(&brain_directory).unwrap();
    fs::write(
        brain_directory.join("mod.rs"),
        "#[path = \"../../outside.rs\"] mod external;",
    )
    .unwrap();
    fs::write(
        directory.path().join("outside.rs"),
        "use crate::harness::context::RepositoryContext;",
    )
    .unwrap();

    let violations = scan_brain_harness_context_dependencies(&brain_directory);

    assert_eq!(violations.len(), 1, "{violations:?}");
}

#[test]
fn crate_root_tools_scanner_follows_external_path_attributes() {
    let directory = tempfile::tempdir().unwrap();
    let source_directory = directory.path().join("src");
    fs::create_dir_all(&source_directory).unwrap();
    let crate_root = source_directory.join("lib.rs");
    fs::write(&crate_root, "#[path = \"tools_alt.rs\"] mod tools;").unwrap();
    fs::write(source_directory.join("tools.rs"), "pub fn safe() {}").unwrap();
    fs::write(
        source_directory.join("tools_alt.rs"),
        "use crate::brain::HiddenDependency;",
    )
    .unwrap();

    let violations = scan_tools_module_from_crate_root(&crate_root);

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("crate::brain")),
        "selected Tools module was not scanned: {violations:?}"
    );
}

#[test]
fn crate_root_tools_scanner_resolves_children_of_path_loaded_tools_siblings() {
    let directory = tempfile::tempdir().unwrap();
    let source_directory = directory.path().join("src");
    let alternate_directory = source_directory.join("tools_alt");
    fs::create_dir_all(&alternate_directory).unwrap();
    let crate_root = source_directory.join("lib.rs");
    fs::write(&crate_root, "#[path = \"tools_alt.rs\"] mod tools;").unwrap();
    fs::write(source_directory.join("tools_alt.rs"), "mod nested;").unwrap();
    fs::write(
        source_directory.join("nested.rs"),
        "use crate::brain::HiddenDependency;",
    )
    .unwrap();
    fs::write(alternate_directory.join("nested.rs"), "pub fn safe() {}").unwrap();

    let violations = scan_tools_module_from_crate_root(&crate_root);

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("crate::brain")),
        "path-loaded Tools module children were resolved under the filename instead of the parent directory: {violations:?}"
    );
}

#[test]
fn module_scanner_uses_mod_rs_semantics_for_path_loaded_child_modules() {
    let directory = tempfile::tempdir().unwrap();
    let source_directory = directory.path().join("src");
    let tools_directory = source_directory.join("tools");
    let path_module_directory = source_directory.join("path_module");
    fs::create_dir_all(&tools_directory).unwrap();
    fs::create_dir_all(&path_module_directory).unwrap();
    fs::write(source_directory.join("lib.rs"), "mod tools;").unwrap();
    let tools_root = source_directory.join("tools.rs");
    fs::write(&tools_root, "#[path = \"path_module.rs\"] mod child;").unwrap();
    fs::write(source_directory.join("path_module.rs"), "mod nested;").unwrap();
    fs::write(
        source_directory.join("nested.rs"),
        "use crate::brain::HiddenDependency;",
    )
    .unwrap();
    fs::write(path_module_directory.join("nested.rs"), "pub fn safe() {}").unwrap();

    let violations = scan_module_file(&tools_root);

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("crate::brain")),
        "#[path]-loaded modules must resolve their children beside the selected file: {violations:?}"
    );
}

#[test]
fn crate_root_tools_scanner_inspects_inline_tools_modules() {
    let directory = tempfile::tempdir().unwrap();
    let source_directory = directory.path().join("src");
    fs::create_dir_all(&source_directory).unwrap();
    let crate_root = source_directory.join("lib.rs");
    fs::write(
        &crate_root,
        "mod tools { use crate::brain::HiddenDependency; }",
    )
    .unwrap();
    fs::write(source_directory.join("tools.rs"), "pub fn safe() {}").unwrap();

    let violations = scan_tools_module_from_crate_root(&crate_root);

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("crate::brain")),
        "inline Tools module was not scanned: {violations:?}"
    );
}

#[test]
fn crate_root_tools_scanner_fails_closed_on_inline_path_attributes() {
    let directory = tempfile::tempdir().unwrap();
    let source_directory = directory.path().join("src");
    fs::create_dir_all(&source_directory).unwrap();
    let crate_root = source_directory.join("lib.rs");
    fs::write(
        &crate_root,
        "#[path = \"tools_alt.rs\"] mod tools { mod nested; }",
    )
    .unwrap();

    let violations = scan_tools_module_from_crate_root(&crate_root);

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("inline Tools module path attribute")),
        "inline Tools path attribute did not fail closed: {violations:?}"
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

    let grouped_root_import = r#"fn build() { format!("{}", { use crate::{brain as root}; let _: Option<root::ModelError> = None; 0 }); }"#;
    let grouped_root_import_violations =
        scan_source(grouped_root_import, "macro_grouped_root_import.rs");
    assert!(
        grouped_root_import_violations
            .iter()
            .any(|violation| violation.contains("macro tokens")),
        "grouped crate-root import inside macro tokens was not rejected: {grouped_root_import_violations:?}"
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

    let shadowed_json_root = scan_source(
        "use hostile_macros as serde_json; fn build() { let _ = serde_json::json!({}); }",
        "shadowed_json_root.rs",
    );
    assert!(
        shadowed_json_root
            .iter()
            .any(|violation| violation.contains("macro import")),
        "shadowed serde_json root was accepted: {shadowed_json_root:?}"
    );

    let shadowed_serde_root = scan_source(
        "use hostile as serde; #[derive(serde::Serialize)] struct Item;",
        "shadowed_serde_root.rs",
    );
    assert!(
        shadowed_serde_root
            .iter()
            .any(|violation| violation.contains("macro import")),
        "shadowed serde root was accepted: {shadowed_serde_root:?}"
    );

    let shadowed_block_json_root = scan_source(
        "fn build() { use hostile_macros as serde_json; let _ = serde_json::json!({}); }",
        "shadowed_block_json_root.rs",
    );
    assert!(
        shadowed_block_json_root
            .iter()
            .any(|violation| violation.contains("macro import")),
        "block-scoped serde_json root was accepted: {shadowed_block_json_root:?}"
    );

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
fn direction_guard_fails_closed_on_unrecognized_rust_syntax() {
    let item_source = "macro facade { () => { use crate::brain::Decision; } }";
    let parsed_item = syn::parse_file(item_source).expect("macro item should parse as verbatim");
    assert!(matches!(
        parsed_item.items.first(),
        Some(syn::Item::Verbatim(_))
    ));
    let item_violations = scan_source(item_source, "unrecognized_item.rs");
    assert!(
        item_violations
            .iter()
            .any(|violation| violation.contains("uninspected production Rust item syntax")),
        "unrecognized item syntax was accepted: {item_violations:?}"
    );

    let type_source = "unsafe<'a> crate::brain::Decision";
    let parsed_type = syn::parse_str::<syn::Type>(type_source)
        .expect("unsupported type syntax should be retained as verbatim");
    assert!(matches!(parsed_type, syn::Type::Verbatim(_)));
    let type_violations = scan_source(
        &format!("type Hidden = {type_source};"),
        "unrecognized_type.rs",
    );
    assert!(
        type_violations
            .iter()
            .any(|violation| violation.contains("uninspected production Rust type syntax")),
        "unrecognized type syntax was accepted: {type_violations:?}"
    );
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
fn crate_root_item_attribute_macros_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");

    for source in [
        "#[opaque::facade] use std::fmt::Debug;",
        "#[opaque::facade] extern crate std;",
        "#[opaque::facade] extern \"C\" { fn generated(); }",
        "#[opaque::facade] pub struct Marker;",
        "#[opaque::facade] mod facade {}",
    ] {
        fs::write(&root, source).unwrap();
        let mut violations = Vec::new();

        scan_crate_root_aliases(&root, &mut violations);

        assert!(
            violations.iter().any(|violation| {
                violation.contains("uninspected production crate-root item attribute")
            }),
            "opaque crate-root item attribute was accepted for `{source}`: {violations:?}"
        );
    }
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
fn crate_root_named_items_and_nested_facades_that_expose_brain_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("lib.rs");
    fs::write(
        &root,
        r#"
            pub(crate) struct Wrapper {
                pub(crate) error: crate::brain::ModelError,
            }
            pub(crate) fn model_error() -> Option<crate::brain::ModelError> { None }
            pub(crate) struct ImplWrapper;
            impl ImplWrapper {
                pub(crate) fn model_error() -> Option<crate::brain::ModelError> { None }
            }
            extern "C" {
                pub(crate) fn foreign_model_error() -> Option<crate::brain::ModelError>;
            }
            pub(crate) mod compatibility {
                pub(crate) struct NestedWrapper {
                    pub(crate) error: crate::brain::ModelError,
                }
                pub(crate) fn nested_model_error() -> Option<crate::brain::ModelError> { None }
            }
        "#,
    )
    .unwrap();
    let mut violations = Vec::new();

    let brain_exports = scan_crate_root_aliases(&root, &mut violations);

    for (facade, item) in [
        ("Wrapper", ""),
        ("model_error", ""),
        ("ImplWrapper", ""),
        ("foreign_model_error", ""),
        ("compatibility", "NestedWrapper"),
        ("compatibility", "nested_model_error"),
    ] {
        let tool_source = if item.is_empty() {
            format!("use crate::{facade};")
        } else {
            format!("use crate::{facade}::{item};")
        };
        let tool_violations =
            scan_source_with_root_exports(&tool_source, "tools_named_facade.rs", 1, &brain_exports);
        assert!(
            !tool_violations.is_empty(),
            "Tools import through `{facade}::{item}` was not rejected; exports: {brain_exports:?}"
        );
    }
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

#[test]
fn module_scanner_revisits_shared_source_in_each_module_context() {
    let directory = tempfile::tempdir().unwrap();
    let source_directory = directory.path().join("src");
    let nested_directory = source_directory.join("tools/nested");
    fs::create_dir_all(&nested_directory).unwrap();
    fs::write(source_directory.join("lib.rs"), "mod tools;").unwrap();
    let tools_root = source_directory.join("tools.rs");
    fs::write(
        &tools_root,
        r#"
            mod nested {
                #[path = "shared.rs"]
                mod deep;
            }
            #[path = "tools/nested/shared.rs"]
            mod shallow;
        "#,
    )
    .unwrap();
    fs::write(
        nested_directory.join("shared.rs"),
        "use super::super::brain::ModelError;",
    )
    .unwrap();

    let violations = scan_module_file(&tools_root);

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("crate::brain")),
        "shared source was not scanned with its shallower module depth: {violations:?}"
    );
}
