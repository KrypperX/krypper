use proc_macro2::{TokenStream, TokenTree};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::{Attribute, Item, Meta, Path as RustPath, UseTree};

const TOOLS_MODULE_DEPTH: usize = 1;

pub(super) fn scan_module_file(root: &Path) -> Vec<String> {
    let mut visited = HashSet::new();
    let mut violations = Vec::new();
    let crate_root = root
        .parent()
        .expect("src/tools.rs has a parent")
        .join("lib.rs");
    let crate_root_brain_exports = scan_crate_root_aliases(&crate_root, &mut violations);
    scan_file(
        root,
        module_directory(root),
        TOOLS_MODULE_DEPTH,
        &mut visited,
        &mut violations,
        &crate_root_brain_exports,
    );
    violations
}

fn scan_file(
    file_path: &Path,
    module_dir: PathBuf,
    module_depth: usize,
    visited: &mut HashSet<PathBuf>,
    violations: &mut Vec<String>,
    crate_root_brain_exports: &HashSet<String>,
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
    scan_items(
        &file.items,
        file_path,
        &module_dir,
        module_depth,
        visited,
        violations,
        crate_root_brain_exports,
    );
}

fn scan_items(
    items: &[Item],
    file_path: &Path,
    module_dir: &Path,
    module_depth: usize,
    visited: &mut HashSet<PathBuf>,
    violations: &mut Vec<String>,
    crate_root_brain_exports: &HashSet<String>,
) {
    let inspected_macro_imports = inspected_macro_imports(items, file_path, violations);
    for item in items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if item_attrs(item)
            .iter()
            .any(|attribute| attribute.path().is_ident("macro_use"))
        {
            violations.push(format!(
                "{}: macro_use imports are not inspected",
                file_path.display()
            ));
        }
        if let Item::Mod(module) = item {
            inspect_module_attributes(
                &module.attrs,
                &inspected_macro_imports,
                module_depth,
                crate_root_brain_exports,
                &file_path.display().to_string(),
                violations,
            );
            if let Some((_, nested)) = &module.content {
                let nested_dir = module_dir.join(module.ident.to_string());
                scan_items(
                    nested,
                    file_path,
                    &nested_dir,
                    module_depth + 1,
                    visited,
                    violations,
                    crate_root_brain_exports,
                );
            } else if let Some(module_path) = resolve_module(file_path, module_dir, module) {
                scan_file(
                    &module_path,
                    module_directory(&module_path),
                    module_depth + 1,
                    visited,
                    violations,
                    crate_root_brain_exports,
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
            module_depth,
            crate_root_brain_exports,
            inspected_macro_imports: &inspected_macro_imports,
            violations,
        };
        visitor.visit_item(item);
    }
}

fn inspect_module_attributes(
    attributes: &[Attribute],
    inspected_macro_imports: &HashSet<String>,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
    origin: &str,
    violations: &mut Vec<String>,
) {
    for attribute in attributes {
        if !attribute_is_inspected(
            attribute,
            inspected_macro_imports,
            module_depth,
            crate_root_brain_exports,
        ) {
            violations.push(format!(
                "{origin}: uninspected production module attribute `{}`",
                path_label(attribute.path())
            ));
        }
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

pub(super) fn scan_source(source: &str, origin: &str) -> Vec<String> {
    scan_source_at_depth(source, origin, TOOLS_MODULE_DEPTH)
}

pub(super) fn scan_source_at_depth(source: &str, origin: &str, module_depth: usize) -> Vec<String> {
    scan_source_with_root_exports(source, origin, module_depth, &HashSet::new())
}

pub(super) fn scan_source_with_root_exports(
    source: &str,
    origin: &str,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
) -> Vec<String> {
    let Ok(file) = syn::parse_file(source) else {
        return vec![format!("{origin}: unable to parse Rust source")];
    };
    let mut violations = Vec::new();
    scan_items(
        &file.items,
        Path::new(origin),
        Path::new("."),
        module_depth,
        &mut HashSet::new(),
        &mut violations,
        crate_root_brain_exports,
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
    module_depth: usize,
    crate_root_brain_exports: &'a HashSet<String>,
    inspected_macro_imports: &'a HashSet<String>,
    violations: &'a mut Vec<String>,
}

impl Visit<'_> for DirectionVisitor<'_> {
    fn visit_item(&mut self, item: &Item) {
        if item_is_test_only(item_attrs(item)) {
            return;
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_attribute(&mut self, attribute: &Attribute) {
        if !attribute_is_inspected(
            attribute,
            self.inspected_macro_imports,
            self.module_depth,
            self.crate_root_brain_exports,
        ) {
            self.violations.push(format!(
                "{}: uninspected production attribute `{}`",
                self.origin,
                path_label(attribute.path())
            ));
        }
        syn::visit::visit_attribute(self, attribute);
    }

    fn visit_item_use(&mut self, item: &syn::ItemUse) {
        inspect_use_tree(
            &item.tree,
            &mut Vec::new(),
            self.module_depth,
            self.crate_root_brain_exports,
            &self.origin,
            self.violations,
        );
    }

    fn visit_path(&mut self, path: &RustPath) {
        let segments = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        if path_starts_with_brain(&segments, self.module_depth)
            || path_starts_with_brain_export(
                &segments,
                self.module_depth,
                self.crate_root_brain_exports,
            )
        {
            self.violations
                .push(format!("{}: qualified path to crate::brain", self.origin));
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &syn::Macro) {
        if mac.path.is_ident("include") {
            self.violations.push(format!(
                "{}: production include! source is not inspected",
                self.origin
            ));
        } else if macro_tokens_contain_forbidden_path(
            &mac.tokens,
            self.module_depth,
            self.crate_root_brain_exports,
        ) {
            self.violations.push(format!(
                "{}: macro tokens contain a crate-root Brain path or alias",
                self.origin
            ));
        } else if !macro_is_inspected(&mac.path, self.inspected_macro_imports) {
            self.violations.push(format!(
                "{}: uninspected production macro `{}`",
                self.origin,
                path_label(&mac.path)
            ));
        }
        syn::visit::visit_macro(self, mac);
    }

    fn visit_item_macro(&mut self, item: &syn::ItemMacro) {
        if item.ident.is_some() {
            self.violations.push(format!(
                "{}: production macro definition is not expanded",
                self.origin
            ));
        }
        syn::visit::visit_item_macro(self, item);
    }

    fn visit_impl_item(&mut self, item: &syn::ImplItem) {
        if impl_item_is_test_only(item) {
            return;
        }
        syn::visit::visit_impl_item(self, item);
    }

    fn visit_trait_item(&mut self, item: &syn::TraitItem) {
        if trait_item_is_test_only(item) {
            return;
        }
        syn::visit::visit_trait_item(self, item);
    }

    fn visit_field(&mut self, field: &syn::Field) {
        if item_is_test_only(&field.attrs) {
            return;
        }
        syn::visit::visit_field(self, field);
    }

    fn visit_variant(&mut self, variant: &syn::Variant) {
        if item_is_test_only(&variant.attrs) {
            return;
        }
        syn::visit::visit_variant(self, variant);
    }
}

fn inspect_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
    origin: &str,
    violations: &mut Vec<String>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            inspect_use_tree(
                &path.tree,
                prefix,
                module_depth,
                crate_root_brain_exports,
                origin,
                violations,
            );
            prefix.pop();
        }
        UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            record_brain_prefix(
                prefix,
                module_depth,
                crate_root_brain_exports,
                origin,
                violations,
            );
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            record_brain_prefix(
                prefix,
                module_depth,
                crate_root_brain_exports,
                origin,
                violations,
            );
            if path_resolves_to_crate_root(prefix, module_depth) {
                violations.push(format!(
                    "{origin}: crate-root import alias `{}` is not checked",
                    rename.rename
                ));
            }
            prefix.pop();
        }
        UseTree::Glob(_) => {
            record_brain_prefix(
                prefix,
                module_depth,
                crate_root_brain_exports,
                origin,
                violations,
            );
            if path_resolves_to_crate_root(prefix, module_depth) {
                violations.push(format!("{origin}: crate-root glob import is not checked"));
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                inspect_use_tree(
                    item,
                    prefix,
                    module_depth,
                    crate_root_brain_exports,
                    origin,
                    violations,
                );
            }
        }
    }
}

fn record_brain_prefix(
    prefix: &[String],
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
    origin: &str,
    violations: &mut Vec<String>,
) {
    if path_starts_with_brain(prefix, module_depth)
        || path_starts_with_brain_export(prefix, module_depth, crate_root_brain_exports)
    {
        violations.push(format!("{origin}: use tree resolves to crate::brain"));
    }
}

fn path_starts_with_brain_export(
    segments: &[String],
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
) -> bool {
    let mut start = match segments.first().map(String::as_str) {
        Some("crate" | "$crate") => 1,
        Some("self") => 1,
        _ => 0,
    };
    let super_count = segments[start..]
        .iter()
        .take_while(|segment| segment.as_str() == "super")
        .count();
    if super_count != 0 {
        if super_count != module_depth {
            return false;
        }
        start += super_count;
    } else if start == 0 {
        return false;
    }
    if segments.get(start).is_some_and(|segment| segment == "self") {
        start += 1;
    }
    segments
        .get(start)
        .is_some_and(|segment| crate_root_brain_exports.contains(segment))
}

fn path_starts_with_brain(segments: &[String], module_depth: usize) -> bool {
    if matches!(
        segments.first().map(String::as_str),
        Some("crate" | "$crate")
    ) {
        let next = usize::from(segments.get(1).is_some_and(|segment| segment == "self")) + 1;
        return segments.get(next).is_some_and(|segment| segment == "brain");
    }
    let start = usize::from(segments.first().is_some_and(|segment| segment == "self"));
    let super_count = segments[start..]
        .iter()
        .take_while(|segment| segment.as_str() == "super")
        .count();
    super_count == module_depth
        && segments
            .get(start + super_count)
            .is_some_and(|segment| segment == "brain")
}

fn path_resolves_to_crate_root(segments: &[String], module_depth: usize) -> bool {
    if segments == ["crate"]
        || segments == ["$crate"]
        || segments == ["crate", "self"]
        || segments == ["$crate", "self"]
    {
        return true;
    }
    let start = usize::from(segments.first().is_some_and(|segment| segment == "self"));
    if start == 1 && module_depth == 0 && segments.len() == 1 {
        return true;
    }
    let super_count = segments[start..]
        .iter()
        .take_while(|segment| segment.as_str() == "super")
        .count();
    super_count == module_depth && start + super_count == segments.len()
}

pub(super) fn scan_crate_root_aliases(
    root: &Path,
    violations: &mut Vec<String>,
) -> HashSet<String> {
    let mut crate_root_brain_exports = HashSet::new();
    let Ok(source) = fs::read_to_string(root) else {
        violations.push(format!(
            "{}: unable to inspect crate root aliases",
            root.display()
        ));
        return crate_root_brain_exports;
    };
    let Ok(file) = syn::parse_file(&source) else {
        violations.push(format!("{}: unable to parse crate root", root.display()));
        return crate_root_brain_exports;
    };
    for item in &file.items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if item_attrs(item)
            .iter()
            .any(|attribute| attribute.path().is_ident("macro_use"))
        {
            violations.push(format!(
                "{}: crate-root macro imports are not inspected",
                root.display()
            ));
        }
        match item {
            Item::Use(item) => {
                inspect_crate_root_alias(
                    &item.tree,
                    &mut Vec::new(),
                    &root.display().to_string(),
                    violations,
                    &mut crate_root_brain_exports,
                );
            }
            Item::Mod(module) if module.ident == "tools" => {
                for attribute in &module.attrs {
                    if !attribute_is_inspected(
                        attribute,
                        &HashSet::new(),
                        0,
                        &crate_root_brain_exports,
                    ) {
                        violations.push(format!(
                            "{}: uninspected crate-root Tools module attribute `{}`",
                            root.display(),
                            path_label(attribute.path())
                        ));
                    }
                }
            }
            Item::ExternCrate(item) if item.ident == "self" && item.rename.is_some() => {
                violations.push(format!(
                    "{}: crate-root extern alias `{}` is not checked",
                    root.display(),
                    item.rename
                        .as_ref()
                        .map(|(_, ident)| ident.to_string())
                        .unwrap_or_default()
                ));
            }
            Item::ExternCrate(item)
                if item
                    .attrs
                    .iter()
                    .any(|attribute| attribute.path().is_ident("macro_use")) =>
            {
                violations.push(format!(
                    "{}: crate-root macro import is not inspected",
                    root.display()
                ));
            }
            Item::Macro(_) => {
                violations.push(format!(
                    "{}: crate-root macro expansion is not inspected",
                    root.display()
                ));
            }
            _ => {}
        }
    }
    crate_root_brain_exports
}

fn inspect_crate_root_alias(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    origin: &str,
    violations: &mut Vec<String>,
    crate_root_brain_exports: &mut HashSet<String>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            inspect_crate_root_alias(
                &path.tree,
                prefix,
                origin,
                violations,
                crate_root_brain_exports,
            );
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            if root_path_starts_with_brain(prefix) {
                crate_root_brain_exports.insert(rename.rename.to_string());
            }
            if path_resolves_to_crate_root(prefix, 0) {
                violations.push(format!(
                    "{origin}: crate-root import alias `{}` is not checked",
                    rename.rename
                ));
            }
            prefix.pop();
        }
        UseTree::Group(group) => {
            for item in &group.items {
                inspect_crate_root_alias(
                    item,
                    prefix,
                    origin,
                    violations,
                    crate_root_brain_exports,
                );
            }
        }
        UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            if root_path_starts_with_brain(prefix) {
                crate_root_brain_exports.insert(name.ident.to_string());
            }
            prefix.pop();
        }
        UseTree::Glob(_) => {
            if root_path_starts_with_brain(prefix) {
                violations.push(format!(
                    "{origin}: crate-root Brain glob import is not inspected"
                ));
            }
        }
    }
}

fn root_path_starts_with_brain(segments: &[String]) -> bool {
    match segments {
        [first, second, ..] if first == "crate" || first == "$crate" || first == "self" => {
            second == "brain"
                || (second == "self" && segments.get(2).is_some_and(|part| part == "brain"))
        }
        [first, ..] => first == "brain",
        [] => false,
    }
}

fn impl_item_is_test_only(item: &syn::ImplItem) -> bool {
    let attributes = match item {
        syn::ImplItem::Const(item) => &item.attrs,
        syn::ImplItem::Fn(item) => &item.attrs,
        syn::ImplItem::Type(item) => &item.attrs,
        syn::ImplItem::Macro(item) => &item.attrs,
        syn::ImplItem::Verbatim(_) => return false,
        _ => return false,
    };
    item_is_test_only(attributes)
}

fn trait_item_is_test_only(item: &syn::TraitItem) -> bool {
    let attributes = match item {
        syn::TraitItem::Const(item) => &item.attrs,
        syn::TraitItem::Fn(item) => &item.attrs,
        syn::TraitItem::Type(item) => &item.attrs,
        syn::TraitItem::Macro(item) => &item.attrs,
        syn::TraitItem::Verbatim(_) => return false,
        _ => return false,
    };
    item_is_test_only(attributes)
}

const PRELUDE_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "cfg",
    "concat",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "dbg",
    "eprint",
    "eprintln",
    "env",
    "file",
    "format",
    "format_args",
    "include_bytes",
    "include_str",
    "line",
    "matches",
    "module_path",
    "option_env",
    "panic",
    "print",
    "println",
    "stringify",
    "todo",
    "unimplemented",
    "unreachable",
    "vec",
    "write",
    "writeln",
];

const BUILTIN_DERIVES: &[&str] = &[
    "Clone",
    "Copy",
    "Debug",
    "Default",
    "Eq",
    "Hash",
    "Ord",
    "PartialEq",
    "PartialOrd",
];

const SERDE_DERIVES: &[&str] = &["Serialize", "Deserialize"];
const SERDE_PATH_OPTIONS: &[&str] = &[
    "crate",
    "default",
    "deserialize",
    "deserialize_with",
    "from",
    "getter",
    "into",
    "remote",
    "serialize",
    "serialize_with",
    "skip_serializing_if",
    "try_from",
    "with",
];

fn attribute_is_inspected(
    attribute: &Attribute,
    imported: &HashSet<String>,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
) -> bool {
    match path_label(attribute.path()).as_str() {
        "allow" | "cfg" | "doc" | "ignore" | "path" | "test" => true,
        "serde" => serde_attribute_is_inspected(attribute, module_depth, crate_root_brain_exports),
        "derive" => {
            let parser = syn::punctuated::Punctuated::<RustPath, syn::Token![,]>::parse_terminated;
            attribute.parse_args_with(parser).is_ok_and(|derives| {
                derives
                    .iter()
                    .all(|derive| derive_is_inspected(derive, imported))
            })
        }
        _ => false,
    }
}

fn serde_attribute_is_inspected(
    attribute: &Attribute,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
) -> bool {
    let parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
    attribute.parse_args_with(parser).is_ok_and(|items| {
        items.iter().all(|item| {
            !serde_meta_contains_brain_path(item, module_depth, crate_root_brain_exports)
        })
    })
}

fn serde_meta_contains_brain_path(
    meta: &Meta,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
) -> bool {
    match meta {
        Meta::Path(_) => false,
        Meta::NameValue(name_value) => {
            let option = path_label(&name_value.path);
            if !SERDE_PATH_OPTIONS.contains(&option.as_str()) && option != "bound" {
                return false;
            }
            match &name_value.value {
                syn::Expr::Lit(expression) => match &expression.lit {
                    syn::Lit::Str(value) => serde_string_contains_brain_path(
                        &value.value(),
                        module_depth,
                        crate_root_brain_exports,
                    ),
                    _ => false,
                },
                expression => {
                    let mut visitor = BrainPathProbe {
                        module_depth,
                        crate_root_brain_exports,
                        found: false,
                    };
                    visitor.visit_expr(expression);
                    visitor.found
                }
            }
        }
        Meta::List(list) => {
            let parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
            match syn::parse::Parser::parse2(parser, list.tokens.clone()) {
                Ok(items) => items.iter().any(|item| {
                    serde_meta_contains_brain_path(item, module_depth, crate_root_brain_exports)
                }),
                Err(_) => true,
            }
        }
    }
}

fn serde_string_contains_brain_path(
    value: &str,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
) -> bool {
    if syn::parse_str::<RustPath>(value).is_ok_and(|path| {
        path_starts_with_brain(
            &path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>(),
            module_depth,
        )
    }) || syn::parse_str::<RustPath>(value).is_ok_and(|path| {
        let segments = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        path_starts_with_brain_export(&segments, module_depth, crate_root_brain_exports)
    }) {
        return true;
    }

    let mut visitor = BrainPathProbe {
        module_depth,
        crate_root_brain_exports,
        found: false,
    };
    if let Ok(ty) = syn::parse_str::<syn::Type>(value) {
        visitor.visit_type(&ty);
    }
    if !visitor.found {
        if let Ok(predicate) = syn::parse_str::<syn::WherePredicate>(value) {
            visitor.visit_where_predicate(&predicate);
        }
    }
    visitor.found
}

struct BrainPathProbe<'a> {
    module_depth: usize,
    crate_root_brain_exports: &'a HashSet<String>,
    found: bool,
}

impl Visit<'_> for BrainPathProbe<'_> {
    fn visit_path(&mut self, path: &RustPath) {
        let segments = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        self.found |= path_starts_with_brain(&segments, self.module_depth)
            || path_starts_with_brain_export(
                &segments,
                self.module_depth,
                self.crate_root_brain_exports,
            );
        syn::visit::visit_path(self, path);
    }
}

fn derive_is_inspected(derive: &RustPath, imported: &HashSet<String>) -> bool {
    let segments: Vec<_> = derive
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    if segments.len() == 1 {
        return BUILTIN_DERIVES.contains(&segments[0].as_str()) || imported.contains(&segments[0]);
    }
    segments.len() == 2
        && matches!(segments[0].as_str(), "serde" | "serde_derive")
        && SERDE_DERIVES.contains(&segments[1].as_str())
}

fn macro_is_inspected(path: &RustPath, imported: &HashSet<String>) -> bool {
    if path.segments.len() == 2
        && path.segments[0].ident == "serde_json"
        && path.segments[1].ident == "json"
    {
        return true;
    }
    path.segments.len() == 1
        && (PRELUDE_MACROS.contains(&path.segments[0].ident.to_string().as_str())
            || imported.contains(&path.segments[0].ident.to_string()))
}

fn inspected_macro_imports(
    items: &[Item],
    file_path: &Path,
    violations: &mut Vec<String>,
) -> HashSet<String> {
    let mut imported = HashSet::new();
    for item in items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        match item {
            Item::Use(item) => inspect_macro_import_tree(
                &item.tree,
                &mut Vec::new(),
                &file_path.display().to_string(),
                &mut imported,
                violations,
            ),
            Item::ExternCrate(item)
                if item
                    .attrs
                    .iter()
                    .any(|attribute| attribute.path().is_ident("macro_use")) =>
            {
                violations.push(format!(
                    "{}: extern crate macro imports are not inspected",
                    file_path.display()
                ));
            }
            _ => {}
        }
    }
    imported
}

fn inspect_macro_import_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    origin: &str,
    imported: &mut HashSet<String>,
    violations: &mut Vec<String>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            inspect_macro_import_tree(&path.tree, prefix, origin, imported, violations);
            prefix.pop();
        }
        UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            inspect_macro_import(prefix, name.ident.to_string(), origin, imported, violations);
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            inspect_macro_import(
                prefix,
                rename.rename.to_string(),
                origin,
                imported,
                violations,
            );
            prefix.pop();
        }
        UseTree::Glob(_) => {
            violations.push(format!("{origin}: glob imports can hide production macros"))
        }
        UseTree::Group(group) => {
            for item in &group.items {
                inspect_macro_import_tree(item, prefix, origin, imported, violations);
            }
        }
    }
}

fn inspect_macro_import(
    source: &[String],
    local_name: String,
    origin: &str,
    imported: &mut HashSet<String>,
    violations: &mut Vec<String>,
) {
    let could_shadow_inspected = PRELUDE_MACROS.contains(&local_name.as_str())
        || SERDE_DERIVES.contains(&local_name.as_str())
        || local_name == "json";
    if !could_shadow_inspected {
        return;
    }
    let trusted = source == ["serde_json", "json"] && local_name == "json"
        || source.len() == 2
            && matches!(source[0].as_str(), "serde" | "serde_derive")
            && SERDE_DERIVES.contains(&source[1].as_str())
            && source[1] == local_name
        || source.len() == 2
            && matches!(source[0].as_str(), "std" | "core")
            && source[1] == local_name
            && PRELUDE_MACROS.contains(&local_name.as_str());
    if trusted {
        imported.insert(local_name);
    } else {
        violations.push(format!(
            "{origin}: macro import `{}` from `{}` is not inspected",
            local_name,
            source.join("::")
        ));
    }
}

fn path_label(path: &RustPath) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn macro_tokens_contain_forbidden_path(
    tokens: &TokenStream,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
) -> bool {
    let tokens: Vec<_> = tokens.clone().into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        if let TokenTree::Group(group) = token {
            if macro_tokens_contain_forbidden_path(
                &group.stream(),
                module_depth,
                crate_root_brain_exports,
            ) {
                return true;
            }
        }
        let Some((segments, end)) = token_path_at(&tokens, index) else {
            continue;
        };
        if path_starts_with_brain(&segments, module_depth)
            || path_starts_with_brain_export(&segments, module_depth, crate_root_brain_exports)
            || path_resolves_to_crate_root(&segments, module_depth)
                && tokens
                    .get(end)
                    .is_some_and(|token| matches!(token, TokenTree::Ident(ident) if ident == "as"))
            || path_resolves_to_crate_root(&segments, module_depth)
                && tokens.get(end).is_some_and(
                    |token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '*'),
                )
            || segments.first().is_some_and(|segment| segment == "include")
                && tokens.get(end).is_some_and(
                    |token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '!'),
                )
        {
            return true;
        }
    }
    false
}

fn token_path_at(tokens: &[TokenTree], start: usize) -> Option<(Vec<String>, usize)> {
    let mut index = start;
    let mut segments = Vec::new();
    if matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '$') {
        let TokenTree::Ident(ident) = tokens.get(index + 1)? else {
            return None;
        };
        if ident != "crate" {
            return None;
        }
        segments.push("$crate".to_owned());
        index += 2;
    } else if let Some(TokenTree::Ident(ident)) = tokens.get(index) {
        segments.push(ident.to_string());
        index += 1;
    } else {
        return None;
    }

    while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ':')
        && matches!(tokens.get(index + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == ':')
    {
        let TokenTree::Ident(ident) = tokens.get(index + 2)? else {
            break;
        };
        segments.push(ident.to_string());
        index += 3;
    }
    Some((segments, index))
}
