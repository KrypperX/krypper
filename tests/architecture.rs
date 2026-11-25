use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::{Attribute, Item, Meta, Path as RustPath, UseTree};

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

fn scan_module_file(root: &Path) -> Vec<String> {
    let mut visited = HashSet::new();
    let mut violations = Vec::new();
    scan_file(root, module_directory(root), &mut visited, &mut violations);
    violations
}

fn scan_file(
    file_path: &Path,
    module_dir: PathBuf,
    visited: &mut HashSet<PathBuf>,
    violations: &mut Vec<String>,
) {
    let normalized = file_path
        .canonicalize()
        .unwrap_or_else(|_| file_path.to_path_buf());
    if !visited.insert(normalized) {
        return;
    }
    let Ok(source) = fs::read_to_string(file_path) else {
        return;
    };
    let Ok(file) = syn::parse_file(&source) else {
        violations.push(format!(
            "{}: unable to parse Rust source",
            file_path.display()
        ));
        return;
    };
    scan_items(&file.items, file_path, &module_dir, visited, violations);
}

fn scan_items(
    items: &[Item],
    file_path: &Path,
    module_dir: &Path,
    visited: &mut HashSet<PathBuf>,
    violations: &mut Vec<String>,
) {
    for item in items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if let Item::Mod(module) = item {
            if let Some((_, nested)) = &module.content {
                let nested_dir = module_dir.join(module.ident.to_string());
                scan_items(nested, file_path, &nested_dir, visited, violations);
            } else if let Some(module_path) = resolve_module(file_path, module_dir, module) {
                scan_file(
                    &module_path,
                    module_directory(&module_path),
                    visited,
                    violations,
                );
            } else {
                violations.push(format!(
                    "{}: unable to resolve production module `{}`",
                    file_path.display(),
                    module.ident
                ));
            }
            continue;
        }
        let mut visitor = DirectionVisitor {
            origin: file_path.display().to_string(),
            violations,
        };
        visitor.visit_item(item);
    }
}

fn resolve_module(file_path: &Path, module_dir: &Path, module: &syn::ItemMod) -> Option<PathBuf> {
    if let Some(path) = module.attrs.iter().find_map(|attribute| {
        let Meta::NameValue(name_value) = &attribute.meta else {
            return None;
        };
        if !name_value.path.is_ident("path") {
            return None;
        }
        let syn::Expr::Lit(expression) = &name_value.value else {
            return None;
        };
        let syn::Lit::Str(value) = &expression.lit else {
            return None;
        };
        Some(value.value())
    }) {
        return file_path.parent().map(|parent| parent.join(path));
    }
    let name = module.ident.to_string();
    let flat = module_dir.join(format!("{name}.rs"));
    if flat.is_file() {
        return Some(flat);
    }
    let nested = module_dir.join(name).join("mod.rs");
    nested.is_file().then_some(nested)
}

fn module_directory(file_path: &Path) -> PathBuf {
    let parent = file_path.parent().unwrap_or_else(|| Path::new("."));
    if file_path.file_name().is_some_and(|name| name == "mod.rs") {
        parent.to_path_buf()
    } else {
        parent.join(file_path.file_stem().unwrap_or_default())
    }
}

fn scan_source(source: &str, origin: &str) -> Vec<String> {
    let Ok(file) = syn::parse_file(source) else {
        return vec![format!("{origin}: unable to parse Rust source")];
    };
    let mut violations = Vec::new();
    scan_items(
        &file.items,
        Path::new(origin),
        Path::new("."),
        &mut HashSet::new(),
        &mut violations,
    );
    violations
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        Item::Verbatim(_) => &[],
        _ => &[],
    }
}

fn item_is_test_only(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<Meta>()
                .is_ok_and(|predicate| cfg_is_test_only(&predicate))
    })
}

fn cfg_is_test_only(predicate: &Meta) -> bool {
    match predicate {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => {
            let parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
            syn::parse::Parser::parse2(parser, list.tokens.clone())
                .is_ok_and(|items| items.iter().any(cfg_is_test_only))
        }
        Meta::List(list) if list.path.is_ident("any") => {
            let parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
            syn::parse::Parser::parse2(parser, list.tokens.clone())
                .is_ok_and(|items| !items.is_empty() && items.iter().all(cfg_is_test_only))
        }
        _ => false,
    }
}

struct DirectionVisitor<'a> {
    origin: String,
    violations: &'a mut Vec<String>,
}

impl Visit<'_> for DirectionVisitor<'_> {
    fn visit_item(&mut self, item: &Item) {
        if item_is_test_only(item_attrs(item)) {
            return;
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_item_use(&mut self, item: &syn::ItemUse) {
        inspect_use_tree(&item.tree, &mut Vec::new(), &self.origin, self.violations);
        syn::visit::visit_item_use(self, item);
    }

    fn visit_path(&mut self, path: &RustPath) {
        if path_starts_with_brain(path) {
            self.violations
                .push(format!("{}: qualified crate::brain path", self.origin));
        }
        syn::visit::visit_path(self, path);
    }
}

fn inspect_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    origin: &str,
    violations: &mut Vec<String>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            record_brain_prefix(prefix, origin, violations);
            inspect_use_tree(&path.tree, prefix, origin, violations);
            prefix.pop();
        }
        UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            record_brain_prefix(prefix, origin, violations);
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            record_brain_prefix(prefix, origin, violations);
            prefix.pop();
        }
        UseTree::Glob(_) => record_brain_prefix(prefix, origin, violations),
        UseTree::Group(group) => {
            for item in &group.items {
                inspect_use_tree(item, prefix, origin, violations);
            }
        }
    }
}

fn record_brain_prefix(prefix: &[String], origin: &str, violations: &mut Vec<String>) {
    if prefix.len() == 2 && prefix[0] == "crate" && prefix[1] == "brain" {
        violations.push(format!("{origin}: crate::brain use tree"));
    }
}

fn path_starts_with_brain(path: &RustPath) -> bool {
    path.segments.len() >= 2
        && path.segments[0].ident == "crate"
        && path.segments[1].ident == "brain"
}
