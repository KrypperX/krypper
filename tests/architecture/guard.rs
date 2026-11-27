use proc_macro2::{Ident, TokenStream, TokenTree};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::{Attribute, Item, Meta, Path as RustPath, UseTree};

const TOOLS_MODULE_DEPTH: usize = 1;
const INSPECTED_CRATE_ROOTS: &[&str] = &["serde", "serde_derive", "serde_json"];
const CORE_ROOT_SELF_ALIAS_PREFIX: &str = "<core-root-self-alias:";
const CORE_ROOT_MODULE_ALIAS_PREFIX: &str = "<core-root-module-alias:";

#[derive(Hash, Eq, PartialEq)]
struct ModuleVisitKey {
    file_path: PathBuf,
    module_dir: PathBuf,
    module_depth: usize,
    inside_inline_module: bool,
}

struct ResolvedModule {
    file_path: PathBuf,
    module_dir: PathBuf,
}

#[derive(Default)]
struct ModuleVisits {
    visited: HashSet<ModuleVisitKey>,
    active_files: HashSet<PathBuf>,
}

fn ident_name(ident: &Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}

fn path_is_ident(path: &RustPath, expected: &str) -> bool {
    path.leading_colon.is_none()
        && path.segments.len() == 1
        && ident_name(&path.segments[0].ident) == expected
}

fn module_has_path_attribute(attributes: &[Attribute]) -> bool {
    attributes
        .iter()
        .any(|attribute| path_is_ident(attribute.path(), "path"))
}

fn is_inspected_crate_root_name(name: &str) -> bool {
    INSPECTED_CRATE_ROOTS.contains(&name)
}

struct ModuleScanContext<'a> {
    file_path: &'a Path,
    module_dir: PathBuf,
    module_depth: usize,
    inside_inline_module: bool,
    crate_root_core_exports: Option<&'a HashSet<String>>,
}

pub(super) fn scan_module_file(root: &Path) -> Vec<String> {
    let mut visits = ModuleVisits::default();
    let mut violations = Vec::new();
    let crate_root = root
        .parent()
        .expect("src/tools.rs has a parent")
        .join("lib.rs");
    let crate_root_brain_exports = scan_crate_root_aliases(&crate_root, &mut violations);
    scan_file(
        ModuleScanContext {
            file_path: root,
            module_dir: module_directory(root),
            module_depth: TOOLS_MODULE_DEPTH,
            inside_inline_module: false,
            crate_root_core_exports: None,
        },
        &mut visits,
        &mut violations,
        &crate_root_brain_exports,
    );
    violations
}

pub(super) fn scan_tools_module_from_crate_root(root: &Path) -> Vec<String> {
    let mut visits = ModuleVisits::default();
    let mut violations = Vec::new();
    let crate_root_brain_exports = scan_crate_root_aliases(root, &mut violations);
    let crate_root_core_exports = scan_crate_root_core_aliases(root, &mut violations);
    let source = match fs::read_to_string(root) {
        Ok(source) => source,
        Err(error) => {
            violations.push(format!(
                "{}: unable to read Rust crate root: {error}",
                root.display()
            ));
            return violations;
        }
    };
    let file = match syn::parse_file(&source) {
        Ok(file) => file,
        Err(_) => {
            violations.push(format!("{}: unable to parse crate root", root.display()));
            return violations;
        }
    };
    let crate_root_module_dir = module_directory(root);
    let mut found_tools_module = false;
    for item in &file.items {
        let Item::Mod(module) = item else {
            continue;
        };
        if ident_name(&module.ident) != "tools" || item_is_test_only(&module.attrs) {
            continue;
        }
        found_tools_module = true;
        inspect_module_attributes(
            &module.attrs,
            &HashSet::new(),
            0,
            &crate_root_brain_exports,
            &root.display().to_string(),
            &mut violations,
        );
        if let Some((_, nested)) = &module.content {
            if module_has_path_attribute(&module.attrs) {
                violations.push(format!(
                    "{}: inline Tools module path attribute is not resolved",
                    root.display()
                ));
            }
            let module_dir = crate_root_module_dir.join(ident_name(&module.ident));
            scan_items(
                nested,
                &ModuleScanContext {
                    file_path: root,
                    module_dir,
                    module_depth: TOOLS_MODULE_DEPTH,
                    inside_inline_module: true,
                    crate_root_core_exports: Some(&crate_root_core_exports),
                },
                &mut visits,
                &mut violations,
                &crate_root_brain_exports,
            );
        } else if let Some(module_path) =
            resolve_module(root, &crate_root_module_dir, module, false)
        {
            scan_file(
                ModuleScanContext {
                    file_path: &module_path.file_path,
                    module_dir: module_path.module_dir,
                    module_depth: TOOLS_MODULE_DEPTH,
                    inside_inline_module: false,
                    crate_root_core_exports: Some(&crate_root_core_exports),
                },
                &mut visits,
                &mut violations,
                &crate_root_brain_exports,
            );
        } else {
            violations.push(format!(
                "{}: unable to resolve production Tools module",
                root.display()
            ));
        }
    }
    if !found_tools_module {
        violations.push(format!(
            "{}: crate root does not declare a production Tools module",
            root.display()
        ));
    }
    violations
}

fn scan_file(
    context: ModuleScanContext<'_>,
    visits: &mut ModuleVisits,
    violations: &mut Vec<String>,
    crate_root_brain_exports: &HashSet<String>,
) {
    let normalized = context
        .file_path
        .canonicalize()
        .unwrap_or_else(|_| context.file_path.to_path_buf());
    if visits.active_files.contains(&normalized) {
        violations.push(format!(
            "{}: recursive module source is not inspected",
            context.file_path.display()
        ));
        return;
    }
    let key = ModuleVisitKey {
        file_path: normalized.clone(),
        module_dir: context
            .module_dir
            .canonicalize()
            .unwrap_or_else(|_| context.module_dir.clone()),
        module_depth: context.module_depth,
        inside_inline_module: context.inside_inline_module,
    };
    if !visits.visited.insert(key) {
        return;
    }
    visits.active_files.insert(normalized.clone());
    let source = match fs::read_to_string(context.file_path) {
        Ok(source) => source,
        Err(error) => {
            violations.push(format!(
                "{}: unable to read Rust source: {error}",
                context.file_path.display()
            ));
            visits.active_files.remove(&normalized);
            return;
        }
    };
    let Ok(file) = syn::parse_file(&source) else {
        violations.push(format!(
            "{}: unable to parse Rust source",
            context.file_path.display()
        ));
        visits.active_files.remove(&normalized);
        return;
    };
    scan_items(
        &file.items,
        &context,
        visits,
        violations,
        crate_root_brain_exports,
    );
    visits.active_files.remove(&normalized);
}

fn scan_items(
    items: &[Item],
    context: &ModuleScanContext<'_>,
    visits: &mut ModuleVisits,
    violations: &mut Vec<String>,
    crate_root_brain_exports: &HashSet<String>,
) {
    let inspected_macro_imports = inspected_macro_imports(items, context.file_path, violations);
    for item in items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if item_attrs(item)
            .iter()
            .any(|attribute| path_is_ident(attribute.path(), "macro_use"))
        {
            violations.push(format!(
                "{}: macro_use imports are not inspected",
                context.file_path.display()
            ));
        }
        if let Item::Mod(module) = item {
            if module_has_path_attribute(&module.attrs) && module.content.is_some() {
                violations.push(format!(
                    "{}: inline module path attribute is not resolved",
                    context.file_path.display()
                ));
            }
            if is_inspected_crate_root_name(&ident_name(&module.ident)) {
                violations.push(format!(
                    "{}: module shadows an inspected macro crate root `{}`",
                    context.file_path.display(),
                    ident_name(&module.ident)
                ));
            }
            inspect_module_attributes(
                &module.attrs,
                &inspected_macro_imports,
                context.module_depth,
                crate_root_brain_exports,
                &context.file_path.display().to_string(),
                violations,
            );
            if let Some((_, nested)) = &module.content {
                let nested_dir = context.module_dir.join(ident_name(&module.ident));
                scan_items(
                    nested,
                    &ModuleScanContext {
                        file_path: context.file_path,
                        module_dir: nested_dir,
                        module_depth: context.module_depth + 1,
                        inside_inline_module: true,
                        crate_root_core_exports: context.crate_root_core_exports,
                    },
                    visits,
                    violations,
                    crate_root_brain_exports,
                );
            } else if let Some(module_path) = resolve_module(
                context.file_path,
                &context.module_dir,
                module,
                context.inside_inline_module,
            ) {
                scan_file(
                    ModuleScanContext {
                        file_path: &module_path.file_path,
                        module_dir: module_path.module_dir,
                        module_depth: context.module_depth + 1,
                        inside_inline_module: false,
                        crate_root_core_exports: context.crate_root_core_exports,
                    },
                    visits,
                    violations,
                    crate_root_brain_exports,
                );
            } else {
                violations.push(format!(
                    "{}: unable to resolve production module `{}`",
                    context.file_path.display(),
                    module.ident
                ));
            }
            continue;
        }
        let mut visitor = DirectionVisitor {
            origin: context.file_path.display().to_string(),
            module_depth: context.module_depth,
            crate_root_brain_exports,
            crate_root_core_exports: context.crate_root_core_exports,
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

fn resolve_module(
    file_path: &Path,
    module_dir: &Path,
    module: &syn::ItemMod,
    inside_inline_module: bool,
) -> Option<ResolvedModule> {
    let path_attributes = module
        .attrs
        .iter()
        .filter(|attribute| path_is_ident(attribute.path(), "path"))
        .collect::<Vec<_>>();
    if path_attributes.len() > 1 {
        return None;
    }
    if let Some(attribute) = path_attributes.first() {
        let Meta::NameValue(name_value) = &attribute.meta else {
            return None;
        };
        let syn::Expr::Lit(expression) = &name_value.value else {
            return None;
        };
        let syn::Lit::Str(value) = &expression.lit else {
            return None;
        };
        let base = if inside_inline_module {
            module_dir
        } else {
            file_path.parent()?
        };
        return Some(ResolvedModule {
            file_path: base.join(value.value()),
            // Rust treats a file selected by `#[path]` like a `mod.rs` file.
            // Its child modules therefore resolve beside that selected file.
            module_dir: base.to_path_buf(),
        });
    }
    let name = ident_name(&module.ident);
    let flat = module_dir.join(format!("{name}.rs"));
    if flat.is_file() {
        return Some(ResolvedModule {
            file_path: flat,
            module_dir: module_dir.join(&name),
        });
    }
    let nested = module_dir.join(name).join("mod.rs");
    nested.is_file().then(|| ResolvedModule {
        module_dir: nested.parent().unwrap_or(module_dir).to_path_buf(),
        file_path: nested,
    })
}

fn module_directory(file_path: &Path) -> PathBuf {
    let parent = file_path.parent().unwrap_or_else(|| Path::new("."));
    if file_path
        .file_name()
        .is_some_and(|name| name == "mod.rs" || name == "lib.rs" || name == "main.rs")
    {
        parent.to_path_buf()
    } else {
        parent.join(file_path.file_stem().unwrap_or_default())
    }
}

pub(super) fn scan_source(source: &str, origin: &str) -> Vec<String> {
    scan_source_at_depth(source, origin, TOOLS_MODULE_DEPTH)
}

pub(super) fn scan_brain_harness_context_dependencies(root: &Path) -> Vec<String> {
    let mut sources = Vec::new();
    let mut violations = Vec::new();
    let root_file = if root.is_dir() {
        root.join("mod.rs")
    } else {
        root.to_path_buf()
    };
    let root_module_dir = module_directory(&root_file);
    collect_brain_module_files(
        &root_file,
        &root_module_dir,
        false,
        Vec::new(),
        &mut HashSet::new(),
        &mut sources,
        &mut violations,
    );
    sources.sort_by(|left, right| left.file_path.cmp(&right.file_path));
    for source in sources {
        violations.extend(scan_brain_context_file(
            &source.file,
            &source.file_path.display().to_string(),
            source.alias_scopes,
        ));
    }
    violations.sort();
    violations
}

fn collect_brain_module_files(
    file_path: &Path,
    module_dir: &Path,
    inside_inline_module: bool,
    parent_alias_scopes: BrainAliasScopes,
    visited: &mut HashSet<BrainModuleVisitKey>,
    sources: &mut Vec<BrainSource>,
    violations: &mut Vec<String>,
) {
    let normalized_file = file_path
        .canonicalize()
        .unwrap_or_else(|_| file_path.to_path_buf());
    let normalized_module_dir = module_dir
        .canonicalize()
        .unwrap_or_else(|_| module_dir.to_path_buf());
    let visit_key = BrainModuleVisitKey {
        file_path: normalized_file.clone(),
        module_dir: normalized_module_dir,
        inside_inline_module,
        parent_alias_scopes: brain_alias_scopes_key(&parent_alias_scopes),
    };
    if !visited.insert(visit_key) {
        return;
    }
    let source = match fs::read_to_string(file_path) {
        Ok(source) => source,
        Err(error) => {
            violations.push(format!(
                "{}: unable to read production Brain source: {error}",
                file_path.display()
            ));
            return;
        }
    };
    let file = match syn::parse_file(&source) {
        Ok(file) => file,
        Err(_) => {
            violations.push(format!(
                "{}: unable to parse Brain Rust source",
                file_path.display()
            ));
            return;
        }
    };
    let mut alias_scopes = parent_alias_scopes;
    alias_scopes.push(BrainAliasScope::module(brain_import_aliases(&file.items)));
    sources.push(BrainSource {
        file_path: file_path.to_path_buf(),
        file: file.clone(),
        alias_scopes: alias_scopes.clone(),
    });
    collect_brain_module_items(
        &file.items,
        &BrainModuleContext {
            file_path,
            module_dir,
            inside_inline_module,
        },
        &alias_scopes,
        visited,
        sources,
        violations,
    );
}

fn collect_brain_module_items(
    items: &[Item],
    context: &BrainModuleContext<'_>,
    alias_scopes: &BrainAliasScopes,
    visited: &mut HashSet<BrainModuleVisitKey>,
    sources: &mut Vec<BrainSource>,
    violations: &mut Vec<String>,
) {
    for item in items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        let Item::Mod(module) = item else {
            continue;
        };
        if let Some((_, nested)) = &module.content {
            let nested_dir = context.module_dir.join(ident_name(&module.ident));
            let mut nested_alias_scopes = alias_scopes.clone();
            nested_alias_scopes.push(BrainAliasScope::module(brain_import_aliases(nested)));
            collect_brain_module_items(
                nested,
                &BrainModuleContext {
                    file_path: context.file_path,
                    module_dir: &nested_dir,
                    inside_inline_module: true,
                },
                &nested_alias_scopes,
                visited,
                sources,
                violations,
            );
        } else if let Some(resolved) = resolve_module(
            context.file_path,
            context.module_dir,
            module,
            context.inside_inline_module,
        ) {
            collect_brain_module_files(
                &resolved.file_path,
                &resolved.module_dir,
                false,
                alias_scopes.clone(),
                visited,
                sources,
                violations,
            );
        } else {
            violations.push(format!(
                "{}: unable to resolve production Brain module `{}`",
                context.file_path.display(),
                module.ident
            ));
        }
    }
}

struct BrainModuleContext<'a> {
    file_path: &'a Path,
    module_dir: &'a Path,
    inside_inline_module: bool,
}

type BrainImportAliases = std::collections::HashMap<String, Vec<Vec<String>>>;
type BrainAliasScopes = Vec<BrainAliasScope>;

#[derive(Clone)]
struct BrainAliasScope {
    aliases: BrainImportAliases,
    is_module: bool,
}

impl BrainAliasScope {
    fn module(aliases: BrainImportAliases) -> Self {
        Self {
            aliases,
            is_module: true,
        }
    }

    fn block(aliases: BrainImportAliases) -> Self {
        Self {
            aliases,
            is_module: false,
        }
    }
}

#[derive(Hash, Eq, PartialEq)]
struct BrainAliasScopeKey {
    is_module: bool,
    aliases: Vec<(String, Vec<Vec<String>>)>,
}

#[derive(Hash, Eq, PartialEq)]
struct BrainModuleVisitKey {
    file_path: PathBuf,
    module_dir: PathBuf,
    inside_inline_module: bool,
    parent_alias_scopes: Vec<BrainAliasScopeKey>,
}

fn brain_alias_scopes_key(scopes: &BrainAliasScopes) -> Vec<BrainAliasScopeKey> {
    scopes
        .iter()
        .map(|scope| {
            let mut aliases = scope
                .aliases
                .iter()
                .map(|(name, targets)| {
                    let mut targets = targets.clone();
                    targets.sort();
                    (name.clone(), targets)
                })
                .collect::<Vec<_>>();
            aliases.sort_by(|left, right| left.0.cmp(&right.0));
            BrainAliasScopeKey {
                is_module: scope.is_module,
                aliases,
            }
        })
        .collect()
}

struct BrainSource {
    file_path: PathBuf,
    file: syn::File,
    alias_scopes: BrainAliasScopes,
}

pub(super) fn scan_brain_context_source(source: &str, origin: &str) -> Vec<String> {
    let Ok(file) = syn::parse_file(source) else {
        return vec![format!("{origin}: unable to parse Brain Rust source")];
    };
    let aliases = vec![BrainAliasScope::module(brain_import_aliases(&file.items))];
    scan_brain_context_file(&file, origin, aliases)
}

fn scan_brain_context_file(
    file: &syn::File,
    origin: &str,
    alias_scopes: BrainAliasScopes,
) -> Vec<String> {
    let mut visitor = BrainContextDependencyVisitor {
        origin: origin.to_owned(),
        alias_scopes,
        violations: Vec::new(),
    };
    visitor.visit_file(file);
    visitor.violations
}

struct BrainContextDependencyVisitor {
    origin: String,
    alias_scopes: BrainAliasScopes,
    violations: Vec<String>,
}

impl Visit<'_> for BrainContextDependencyVisitor {
    fn visit_item(&mut self, item: &Item) {
        if item_is_test_only(item_attrs(item)) {
            return;
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_item_use(&mut self, item: &syn::ItemUse) {
        inspect_brain_context_use_tree(
            &item.tree,
            &mut Vec::new(),
            &self.origin,
            &self.alias_scopes,
            &mut self.violations,
        );
    }

    fn visit_path(&mut self, path: &RustPath) {
        let segments = path
            .segments
            .iter()
            .map(|segment| ident_name(&segment.ident))
            .collect::<Vec<_>>();
        if path_resolves_to_harness_context(&segments, &self.alias_scopes) {
            self.violations.push(format!(
                "{}: production Brain path imports Harness context",
                self.origin
            ));
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &syn::Macro) {
        if path_is_ident(&mac.path, "include")
            || token_stream_contains_ident(&mac.tokens, "harness")
            || macro_references_harness_alias(&mac.tokens, &self.alias_scopes)
        {
            self.violations.push(format!(
                "{}: production Brain macro hides a Harness context dependency",
                self.origin
            ));
        }
        syn::visit::visit_macro(self, mac);
    }

    fn visit_item_mod(&mut self, module: &syn::ItemMod) {
        for attribute in &module.attrs {
            self.visit_attribute(attribute);
        }
        if let Some((_, nested)) = &module.content {
            self.alias_scopes
                .push(BrainAliasScope::module(brain_import_aliases(nested)));
            for item in nested {
                self.visit_item(item);
            }
            self.alias_scopes.pop();
        }
    }

    fn visit_block(&mut self, block: &syn::Block) {
        self.alias_scopes
            .push(BrainAliasScope::block(brain_block_import_aliases(
                &block.stmts,
            )));
        syn::visit::visit_block(self, block);
        self.alias_scopes.pop();
    }
}

fn token_stream_contains_ident(tokens: &TokenStream, expected: &str) -> bool {
    tokens.clone().into_iter().any(|token| match token {
        TokenTree::Ident(ident) => ident_name(&ident) == expected,
        TokenTree::Group(group) => token_stream_contains_ident(&group.stream(), expected),
        TokenTree::Punct(_) | TokenTree::Literal(_) => false,
    })
}

fn macro_references_harness_alias(tokens: &TokenStream, alias_scopes: &BrainAliasScopes) -> bool {
    fn collect_paths(tokens: &TokenStream, paths: &mut Vec<(Vec<String>, bool)>) {
        let tokens = tokens.clone().into_iter().collect::<Vec<_>>();
        let mut index = 0;
        while index < tokens.len() {
            if let TokenTree::Group(group) = &tokens[index] {
                collect_paths(&group.stream(), paths);
            }
            let TokenTree::Ident(ident) = &tokens[index] else {
                index += 1;
                continue;
            };
            let mut segments = vec![ident_name(ident)];
            let mut cursor = index + 1;
            while cursor + 2 < tokens.len()
                && matches!(&tokens[cursor], TokenTree::Punct(punct) if punct.as_char() == ':')
                && matches!(&tokens[cursor + 1], TokenTree::Punct(punct) if punct.as_char() == ':')
            {
                let TokenTree::Ident(next) = &tokens[cursor + 2] else {
                    break;
                };
                segments.push(ident_name(next));
                cursor += 3;
            }
            let is_glob = matches!(
                (tokens.get(cursor), tokens.get(cursor + 1), tokens.get(cursor + 2)),
                (
                    Some(TokenTree::Punct(first_colon)),
                    Some(TokenTree::Punct(second_colon)),
                    Some(TokenTree::Punct(glob))
                ) if first_colon.as_char() == ':'
                    && second_colon.as_char() == ':'
                    && glob.as_char() == '*'
            );
            if segments.len() > 1 || is_glob {
                paths.push((segments, is_glob));
            }
            index += 1;
        }
    }

    let mut paths = Vec::new();
    collect_paths(tokens, &mut paths);
    paths.into_iter().any(|(path, is_glob)| {
        path_resolves_to_harness_context(&path, alias_scopes)
            || (is_glob && path_resolves_to_harness_module(&path, alias_scopes))
    })
}

fn inspect_brain_context_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    origin: &str,
    alias_scopes: &BrainAliasScopes,
    violations: &mut Vec<String>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_name(&path.ident));
            inspect_brain_context_use_tree(&path.tree, prefix, origin, alias_scopes, violations);
            prefix.pop();
        }
        UseTree::Name(name) => {
            prefix.push(ident_name(&name.ident));
            if path_resolves_to_harness_context(prefix, alias_scopes) {
                violations.push(format!(
                    "{origin}: production Brain import uses Harness context"
                ));
            }
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(ident_name(&rename.ident));
            if path_resolves_to_harness_context(prefix, alias_scopes) {
                violations.push(format!(
                    "{origin}: production Brain import uses Harness context"
                ));
            }
            prefix.pop();
        }
        UseTree::Glob(_) => {
            if path_resolves_to_harness_context(prefix, alias_scopes)
                || path_resolves_to_harness_module(prefix, alias_scopes)
            {
                violations.push(format!(
                    "{origin}: production Brain glob import uses Harness context"
                ));
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                inspect_brain_context_use_tree(item, prefix, origin, alias_scopes, violations);
            }
        }
    }
}

fn path_contains_harness_context(segments: &[String]) -> bool {
    segments
        .windows(2)
        .any(|pair| pair[0] == "harness" && pair[1] == "context")
}

fn brain_import_aliases(items: &[Item]) -> BrainImportAliases {
    let mut aliases = BrainImportAliases::default();
    for item in items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if let Item::Use(item_use) = item {
            collect_brain_import_aliases(&item_use.tree, &mut Vec::new(), &mut aliases);
        }
    }
    aliases
}

fn brain_block_import_aliases(statements: &[syn::Stmt]) -> BrainImportAliases {
    let mut aliases = BrainImportAliases::default();
    for statement in statements {
        let syn::Stmt::Item(item) = statement else {
            continue;
        };
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if let Item::Use(item_use) = item {
            collect_brain_import_aliases(&item_use.tree, &mut Vec::new(), &mut aliases);
        }
    }
    aliases
}

fn collect_brain_import_aliases(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    aliases: &mut BrainImportAliases,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_name(&path.ident));
            collect_brain_import_aliases(&path.tree, prefix, aliases);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let binding = ident_name(&name.ident);
            let mut target = prefix.clone();
            target.push(binding.clone());
            aliases.entry(binding).or_default().push(target);
        }
        UseTree::Rename(rename) => {
            let mut target = prefix.clone();
            let imported = ident_name(&rename.ident);
            if imported != "self" {
                target.push(imported);
            }
            aliases
                .entry(ident_name(&rename.rename))
                .or_default()
                .push(target);
        }
        UseTree::Glob(_) => {}
        UseTree::Group(group) => {
            for item in &group.items {
                collect_brain_import_aliases(item, prefix, aliases);
            }
        }
    }
}

fn path_resolves_to_harness_context(segments: &[String], alias_scopes: &BrainAliasScopes) -> bool {
    fn resolve(
        segments: &[String],
        alias_scopes: &BrainAliasScopes,
        seen: &mut HashSet<(usize, String)>,
    ) -> bool {
        if path_contains_harness_context(segments) {
            return true;
        }
        let Some((alias_index, scope_index)) = alias_binding_location(segments, alias_scopes)
        else {
            return false;
        };
        let alias = &segments[alias_index];
        let Some(targets) = alias_scopes[scope_index].aliases.get(alias) else {
            return false;
        };
        let key = (scope_index, alias.clone());
        if !seen.insert(key.clone()) {
            return false;
        }
        let found = targets.iter().any(|target| {
            let mut expanded = target.clone();
            expanded.extend_from_slice(&segments[alias_index + 1..]);
            resolve(&expanded, alias_scopes, &mut seen.clone())
        });
        seen.remove(&key);
        found
    }

    resolve(segments, alias_scopes, &mut HashSet::new())
}

fn path_resolves_to_harness_module(segments: &[String], alias_scopes: &BrainAliasScopes) -> bool {
    fn resolve(
        segments: &[String],
        alias_scopes: &BrainAliasScopes,
        seen: &mut HashSet<(usize, String)>,
    ) -> bool {
        if segments.last().is_some_and(|segment| segment == "harness") {
            return true;
        }
        let Some((alias_index, scope_index)) = alias_binding_location(segments, alias_scopes)
        else {
            return false;
        };
        let alias = &segments[alias_index];
        let Some(targets) = alias_scopes[scope_index].aliases.get(alias) else {
            return false;
        };
        let key = (scope_index, alias.clone());
        if !seen.insert(key.clone()) {
            return false;
        }
        let found = targets.iter().any(|target| {
            let mut expanded = target.clone();
            expanded.extend_from_slice(&segments[alias_index + 1..]);
            resolve(&expanded, alias_scopes, &mut seen.clone())
        });
        seen.remove(&key);
        found
    }

    resolve(segments, alias_scopes, &mut HashSet::new())
}

fn alias_binding_location(
    segments: &[String],
    alias_scopes: &BrainAliasScopes,
) -> Option<(usize, usize)> {
    let qualifier_count = segments
        .iter()
        .take_while(|segment| *segment == "self" || *segment == "super")
        .count();
    if qualifier_count > 0 {
        let alias = segments.get(qualifier_count)?;
        let mut scope_index = alias_scopes.iter().rposition(|scope| scope.is_module)?;
        for qualifier in &segments[..qualifier_count] {
            if qualifier == "super" {
                scope_index = alias_scopes[..scope_index]
                    .iter()
                    .rposition(|scope| scope.is_module)?;
            }
        }
        return alias_scopes[scope_index]
            .aliases
            .contains_key(alias)
            .then_some((qualifier_count, scope_index));
    }

    let alias = segments.first()?;
    let scope_index = alias_scopes
        .iter()
        .rposition(|scope| scope.aliases.contains_key(alias))?;
    Some((0, scope_index))
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
        &ModuleScanContext {
            file_path: Path::new(origin),
            module_dir: Path::new(".").to_path_buf(),
            module_depth,
            inside_inline_module: false,
            crate_root_core_exports: None,
        },
        &mut ModuleVisits::default(),
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
        path_is_ident(attribute.path(), "cfg")
            && attribute
                .parse_args::<Meta>()
                .is_ok_and(|predicate| cfg_is_test_only(&predicate))
    })
}

fn cfg_is_test_only(predicate: &Meta) -> bool {
    match predicate {
        Meta::Path(path) => path_is_ident(path, "test"),
        Meta::List(list) if path_is_ident(&list.path, "all") => {
            let parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
            syn::parse::Parser::parse2(parser, list.tokens.clone())
                .is_ok_and(|items| items.iter().any(cfg_is_test_only))
        }
        Meta::List(list) if path_is_ident(&list.path, "any") => {
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
    crate_root_core_exports: Option<&'a HashSet<String>>,
    inspected_macro_imports: &'a HashSet<String>,
    violations: &'a mut Vec<String>,
}

impl DirectionVisitor<'_> {
    fn reject_uninspected_syntax(&mut self, syntax: &str) {
        self.violations.push(format!(
            "{}: uninspected production Rust {syntax}",
            self.origin
        ));
    }
}

impl Visit<'_> for DirectionVisitor<'_> {
    fn visit_item(&mut self, item: &Item) {
        if item_is_test_only(item_attrs(item)) {
            return;
        }
        if matches!(item, Item::Verbatim(_)) {
            self.reject_uninspected_syntax("item syntax");
            return;
        }
        if let Item::Mod(module) = item {
            if is_inspected_crate_root_name(&ident_name(&module.ident)) {
                self.violations.push(format!(
                    "{}: module shadows an inspected macro crate root `{}`",
                    self.origin,
                    ident_name(&module.ident)
                ));
            }
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_foreign_item(&mut self, item: &syn::ForeignItem) {
        if matches!(item, syn::ForeignItem::Verbatim(_)) {
            self.reject_uninspected_syntax("foreign-item syntax");
            return;
        }
        syn::visit::visit_foreign_item(self, item);
    }

    fn visit_stmt(&mut self, statement: &syn::Stmt) {
        let attributes = match statement {
            syn::Stmt::Local(local) => &local.attrs[..],
            syn::Stmt::Item(item) => item_attrs(item),
            syn::Stmt::Macro(mac) => &mac.attrs[..],
            syn::Stmt::Expr(expression, _) if expression_is_test_only(expression) => return,
            syn::Stmt::Expr(_, _) => &[],
        };
        if item_is_test_only(attributes) {
            return;
        }
        syn::visit::visit_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &syn::Expr) {
        if expression_is_test_only(expression) {
            return;
        }
        if matches!(expression, syn::Expr::Verbatim(_)) {
            self.reject_uninspected_syntax("expression syntax");
            return;
        }
        syn::visit::visit_expr(self, expression);
    }

    fn visit_impl_item(&mut self, item: &syn::ImplItem) {
        if matches!(item, syn::ImplItem::Verbatim(_)) {
            self.reject_uninspected_syntax("impl-item syntax");
            return;
        }
        if impl_item_is_test_only(item) {
            return;
        }
        syn::visit::visit_impl_item(self, item);
    }

    fn visit_lit(&mut self, literal: &syn::Lit) {
        if matches!(literal, syn::Lit::Verbatim(_)) {
            self.reject_uninspected_syntax("literal syntax");
            return;
        }
        syn::visit::visit_lit(self, literal);
    }

    fn visit_pat(&mut self, pattern: &syn::Pat) {
        if matches!(pattern, syn::Pat::Verbatim(_)) {
            self.reject_uninspected_syntax("pattern syntax");
            return;
        }
        syn::visit::visit_pat(self, pattern);
    }

    fn visit_trait_item(&mut self, item: &syn::TraitItem) {
        if matches!(item, syn::TraitItem::Verbatim(_)) {
            self.reject_uninspected_syntax("trait-item syntax");
            return;
        }
        if trait_item_is_test_only(item) {
            return;
        }
        syn::visit::visit_trait_item(self, item);
    }

    fn visit_type(&mut self, ty: &syn::Type) {
        if matches!(ty, syn::Type::Verbatim(_)) {
            self.reject_uninspected_syntax("type syntax");
            return;
        }
        syn::visit::visit_type(self, ty);
    }

    fn visit_type_param_bound(&mut self, bound: &syn::TypeParamBound) {
        if matches!(bound, syn::TypeParamBound::Verbatim(_)) {
            self.reject_uninspected_syntax("type-bound syntax");
            return;
        }
        syn::visit::visit_type_param_bound(self, bound);
    }

    fn visit_attribute(&mut self, attribute: &Attribute) {
        let serde_core_path = self.crate_root_core_exports.is_some_and(|exports| {
            serde_attribute_contains_core_path(attribute, self.module_depth, exports)
        });
        if serde_core_path {
            self.violations.push(format!(
                "{}: production Tools attribute depends on Core",
                self.origin
            ));
        } else if !attribute_is_inspected(
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
        for attribute in &item.attrs {
            self.visit_attribute(attribute);
        }
        inspect_macro_import_tree(
            &item.tree,
            &mut Vec::new(),
            &self.origin,
            &mut HashSet::new(),
            self.violations,
        );
        inspect_use_tree(
            &item.tree,
            &mut Vec::new(),
            self.module_depth,
            self.crate_root_brain_exports,
            &self.origin,
            self.violations,
        );
        if let Some(exports) = self.crate_root_core_exports {
            inspect_core_use_tree(
                &item.tree,
                &mut Vec::new(),
                self.module_depth,
                exports,
                &self.origin,
                self.violations,
            );
        }
    }

    fn visit_item_extern_crate(&mut self, item: &syn::ItemExternCrate) {
        if ident_name(&item.ident) == "self" && item.rename.is_some() {
            self.violations
                .push(format!("{}: self crate alias is not checked", self.origin));
        }
        if let Some((_, rename)) = &item.rename {
            if is_inspected_crate_root_name(&ident_name(rename)) {
                self.violations.push(format!(
                    "{}: extern crate alias shadows an inspected macro crate root `{}`",
                    self.origin,
                    ident_name(rename)
                ));
            }
        }
        syn::visit::visit_item_extern_crate(self, item);
    }

    fn visit_path(&mut self, path: &RustPath) {
        let segments = path
            .segments
            .iter()
            .map(|segment| ident_name(&segment.ident))
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
        if self.crate_root_core_exports.is_some_and(|exports| {
            path_starts_with_core(&segments, self.module_depth)
                || path_starts_with_core_export(&segments, self.module_depth, exports)
        }) {
            self.violations.push(format!(
                "{}: production Tools path depends on Core",
                self.origin
            ));
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &syn::Macro) {
        if path_is_ident(&mac.path, "include") {
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
        } else if self.crate_root_core_exports.is_some_and(|exports| {
            macro_tokens_contain_core_path(&mac.tokens, self.module_depth, exports)
        }) {
            self.violations.push(format!(
                "{}: production Tools macro tokens depend on Core",
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

fn expression_is_test_only(expression: &syn::Expr) -> bool {
    match expression {
        syn::Expr::Block(expression) => item_is_test_only(&expression.attrs),
        _ => false,
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
            prefix.push(ident_name(&path.ident));
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
            prefix.push(ident_name(&name.ident));
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
            prefix.push(ident_name(&rename.ident));
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
                    ident_name(&rename.rename)
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

fn scan_crate_root_core_aliases(root: &Path, violations: &mut Vec<String>) -> HashSet<String> {
    let mut exports = HashSet::new();
    let source = match fs::read_to_string(root) {
        Ok(source) => source,
        Err(error) => {
            violations.push(format!(
                "{}: unable to inspect crate-root Core aliases: {error}",
                root.display()
            ));
            return exports;
        }
    };
    let file = match syn::parse_file(&source) {
        Ok(file) => file,
        Err(_) => {
            violations.push(format!("{}: unable to parse crate root", root.display()));
            return exports;
        }
    };

    if file.items.iter().any(|item| {
        matches!(item, Item::Mod(module)
            if ident_name(&module.ident) == "core"
                && !item_is_test_only(&module.attrs))
    }) {
        exports.insert("core".to_owned());
    }
    let core_glob_exports = file.items.iter().find_map(|item| match item {
        Item::Mod(module)
            if ident_name(&module.ident) == "core" && !item_is_test_only(&module.attrs) =>
        {
            Some(module_public_glob_exports(
                module,
                root,
                &module_directory(root),
                false,
            ))
        }
        _ => None,
    });
    let mut ignored_macro_import_violations = Vec::new();
    let root_inspected_macros =
        inspected_macro_imports(&file.items, root, &mut ignored_macro_import_violations);
    let mut root_module_aliases = HashMap::new();

    loop {
        let previous_counts = (exports.len(), root_module_aliases.len());
        let root_module_glob_exports = file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Mod(module) if !item_is_test_only(&module.attrs) => Some((
                    ident_name(&module.ident),
                    module_core_glob_exports(
                        module,
                        root,
                        &module_directory(root),
                        false,
                        1,
                        &exports,
                        &mut HashSet::new(),
                    ),
                )),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        for item in &file.items {
            if item_is_test_only(item_attrs(item)) {
                continue;
            }
            match item {
                Item::Use(item) => {
                    let origin = root.display().to_string();
                    let mut context = CoreAliasContext {
                        core_glob_exports: core_glob_exports.as_ref().and_then(Option::as_ref),
                        root_module_glob_exports: &root_module_glob_exports,
                        root_module_aliases: &mut root_module_aliases,
                        origin: &origin,
                        violations,
                    };
                    collect_core_aliases(&item.tree, &mut Vec::new(), &mut exports, &mut context);
                }
                Item::Mod(module)
                    if !matches!(ident_name(&module.ident).as_str(), "core" | "tools")
                        && module_contains_core_dependency(
                            module,
                            root,
                            &module_directory(root),
                            false,
                            1,
                            &exports,
                            &mut HashSet::new(),
                        ) =>
                {
                    exports.insert(ident_name(&module.ident));
                }
                Item::ExternCrate(item)
                    if ident_name(&item.ident) == "self" && item.rename.is_some() =>
                {
                    if let Some((_, alias)) = &item.rename {
                        exports.insert(core_root_self_alias_key(&ident_name(alias)));
                    }
                }
                _ => {
                    if let Some(name) = root_item_name(item) {
                        let mut probe = CoreDependencyProbe {
                            module_depth: 0,
                            crate_root_core_exports: &exports,
                            inspected_macro_imports: &root_inspected_macros,
                            found: false,
                        };
                        probe.visit_item(item);
                        if probe.found {
                            exports.insert(ident_name(name));
                        }
                    }
                }
            }
        }
        if previous_counts == (exports.len(), root_module_aliases.len()) {
            break;
        }
    }
    exports
}

fn module_contains_core_dependency(
    module: &syn::ItemMod,
    source_file: &Path,
    module_dir: &Path,
    inside_inline_module: bool,
    module_depth: usize,
    crate_root_core_exports: &HashSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> bool {
    if item_is_test_only(&module.attrs) {
        return false;
    }
    if module.attrs.iter().any(|attribute| {
        !attribute_is_inspected(attribute, &HashSet::new(), module_depth, &HashSet::new())
            || serde_attribute_contains_core_path(attribute, module_depth, crate_root_core_exports)
    }) {
        return true;
    }
    if let Some((_, items)) = &module.content {
        if module_has_path_attribute(&module.attrs) {
            return true;
        }
        return items_contain_core_dependency(
            items,
            source_file,
            &module_dir.join(ident_name(&module.ident)),
            true,
            module_depth,
            crate_root_core_exports,
            visited,
        );
    }

    let Some(resolved_module) =
        resolve_module(source_file, module_dir, module, inside_inline_module)
    else {
        return true;
    };
    let normalized = resolved_module
        .file_path
        .canonicalize()
        .unwrap_or_else(|_| resolved_module.file_path.clone());
    if !visited.insert(normalized.clone()) {
        return true;
    }
    let result = fs::read_to_string(&resolved_module.file_path)
        .ok()
        .and_then(|source| syn::parse_file(&source).ok())
        .is_none_or(|file| {
            items_contain_core_dependency(
                &file.items,
                &resolved_module.file_path,
                &resolved_module.module_dir,
                false,
                module_depth,
                crate_root_core_exports,
                visited,
            )
        });
    visited.remove(&normalized);
    result
}

fn items_contain_core_dependency(
    items: &[Item],
    source_file: &Path,
    module_dir: &Path,
    inside_inline_module: bool,
    module_depth: usize,
    crate_root_core_exports: &HashSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> bool {
    let mut macro_import_violations = Vec::new();
    let inspected_macros =
        inspected_macro_imports(items, source_file, &mut macro_import_violations);
    if !macro_import_violations.is_empty() {
        return true;
    }
    items.iter().any(|item| {
        if item_is_test_only(item_attrs(item)) {
            return false;
        }
        if let Item::Mod(module) = item {
            return module_contains_core_dependency(
                module,
                source_file,
                module_dir,
                inside_inline_module,
                module_depth + 1,
                crate_root_core_exports,
                visited,
            );
        }
        if matches!(item, Item::Macro(_))
            || item_attrs(item).iter().any(|attribute| {
                !attribute_is_inspected(attribute, &inspected_macros, module_depth, &HashSet::new())
                    || serde_attribute_contains_core_path(
                        attribute,
                        module_depth,
                        crate_root_core_exports,
                    )
            })
        {
            return true;
        }
        let mut probe = CoreDependencyProbe {
            module_depth,
            crate_root_core_exports,
            inspected_macro_imports: &inspected_macros,
            found: false,
        };
        probe.visit_item(item);
        probe.found
    })
}

struct CoreDependencyProbe<'a> {
    module_depth: usize,
    crate_root_core_exports: &'a HashSet<String>,
    inspected_macro_imports: &'a HashSet<String>,
    found: bool,
}

impl Visit<'_> for CoreDependencyProbe<'_> {
    fn visit_item(&mut self, item: &Item) {
        if item_is_test_only(item_attrs(item)) {
            return;
        }
        if matches!(item, Item::Verbatim(_)) {
            self.found = true;
            return;
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_item_use(&mut self, item: &syn::ItemUse) {
        let mut violations = Vec::new();
        inspect_core_use_tree(
            &item.tree,
            &mut Vec::new(),
            self.module_depth,
            self.crate_root_core_exports,
            "facade item",
            &mut violations,
        );
        self.found |= !violations.is_empty();
    }

    fn visit_stmt(&mut self, statement: &syn::Stmt) {
        let attributes = match statement {
            syn::Stmt::Local(local) => &local.attrs[..],
            syn::Stmt::Item(item) => item_attrs(item),
            syn::Stmt::Macro(mac) => &mac.attrs[..],
            syn::Stmt::Expr(expression, _) if expression_is_test_only(expression) => return,
            syn::Stmt::Expr(_, _) => &[],
        };
        if item_is_test_only(attributes) {
            return;
        }
        syn::visit::visit_stmt(self, statement);
    }

    fn visit_expr(&mut self, expression: &syn::Expr) {
        if expression_is_test_only(expression) {
            return;
        }
        syn::visit::visit_expr(self, expression);
    }

    fn visit_attribute(&mut self, attribute: &Attribute) {
        if serde_attribute_contains_core_path(
            attribute,
            self.module_depth,
            self.crate_root_core_exports,
        ) || !attribute_is_inspected(
            attribute,
            self.inspected_macro_imports,
            self.module_depth,
            &HashSet::new(),
        ) {
            self.found = true;
        }
        syn::visit::visit_attribute(self, attribute);
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

    fn visit_path(&mut self, path: &RustPath) {
        let segments = path
            .segments
            .iter()
            .map(|segment| ident_name(&segment.ident))
            .collect::<Vec<_>>();
        self.found |= path_starts_with_core(&segments, self.module_depth)
            || path_starts_with_core_export(
                &segments,
                self.module_depth,
                self.crate_root_core_exports,
            );
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &syn::Macro) {
        self.found |= path_is_ident(&mac.path, "include")
            || macro_tokens_contain_core_path(
                &mac.tokens,
                self.module_depth,
                self.crate_root_core_exports,
            )
            || !macro_is_inspected(&mac.path, self.inspected_macro_imports);
        if !self.found {
            syn::visit::visit_macro(self, mac);
        }
    }

    fn visit_item_macro(&mut self, _item: &syn::ItemMacro) {
        self.found = true;
    }
}

struct CoreAliasContext<'a> {
    core_glob_exports: Option<&'a HashSet<String>>,
    root_module_glob_exports: &'a HashMap<String, Option<HashSet<String>>>,
    root_module_aliases: &'a mut HashMap<String, String>,
    origin: &'a str,
    violations: &'a mut Vec<String>,
}

fn collect_core_aliases(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    exports: &mut HashSet<String>,
    context: &mut CoreAliasContext<'_>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_name(&path.ident));
            collect_core_aliases(&path.tree, prefix, exports, context);
            prefix.pop();
        }
        UseTree::Name(name) => {
            prefix.push(ident_name(&name.ident));
            if root_path_starts_with_core_or_export(prefix, exports) {
                exports.insert(ident_name(&name.ident));
            }
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(ident_name(&rename.ident));
            if root_path_starts_with_core_or_export(prefix, exports) {
                exports.insert(ident_name(&rename.rename));
                if root_path_is_core_module(prefix, exports) {
                    exports.insert(core_root_module_alias_key(&ident_name(&rename.rename)));
                } else if let Some(module_name) =
                    root_path_root_module_name(prefix, exports, context.root_module_aliases)
                {
                    context
                        .root_module_aliases
                        .insert(ident_name(&rename.rename), module_name);
                }
            } else if path_resolves_to_crate_root(prefix, 0) {
                exports.insert(core_root_self_alias_key(&ident_name(&rename.rename)));
            }
            prefix.pop();
        }
        UseTree::Glob(_) => {
            if root_path_starts_with_core_or_export(prefix, exports) {
                if root_path_is_core_module(prefix, exports) {
                    if let Some(names) = context.core_glob_exports {
                        exports.extend(names.iter().cloned());
                    } else if !context.violations.iter().any(|violation| {
                        violation.contains("Core glob export names are not inspectable")
                    }) {
                        context.violations.push(format!(
                            "{}: Core glob export names are not inspectable",
                            context.origin
                        ));
                    }
                } else if let Some(module_name) =
                    root_path_root_module_name(prefix, exports, context.root_module_aliases)
                {
                    match context.root_module_glob_exports.get(&module_name) {
                        Some(Some(names)) => exports.extend(names.iter().cloned()),
                        Some(None) | None => {
                            if !context.violations.iter().any(|violation| {
                                violation.contains("Core facade glob exports are not inspectable")
                            }) {
                                context.violations.push(format!(
                                    "{}: Core facade glob exports are not inspectable",
                                    context.origin
                                ));
                            }
                        }
                    }
                } else {
                    if !context.violations.iter().any(|violation| {
                        violation.contains("Core facade glob exports are not inspectable")
                    }) {
                        context.violations.push(format!(
                            "{}: Core facade glob exports are not inspectable",
                            context.origin
                        ));
                    }
                }
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_core_aliases(item, prefix, exports, context);
            }
        }
    }
}

fn root_path_is_core_module(segments: &[String], exports: &HashSet<String>) -> bool {
    let mut start = match segments.first().map(String::as_str) {
        Some("crate" | "$crate" | "self") => 1,
        Some(segment) if exports.contains(&core_root_self_alias_key(segment)) => 1,
        Some(segment) if exports.contains(segment) => 0,
        _ => return false,
    };
    if segments
        .get(start)
        .is_some_and(|segment| exports.contains(&core_root_self_alias_key(segment)))
    {
        start += 1;
    }
    segments.len() == start + 1
        && segments.get(start).is_some_and(|segment| {
            segment == "core" || exports.contains(&core_root_module_alias_key(segment))
        })
}

fn root_path_root_module_name(
    segments: &[String],
    exports: &HashSet<String>,
    aliases: &HashMap<String, String>,
) -> Option<String> {
    let mut start = match segments.first().map(String::as_str) {
        Some("crate" | "$crate" | "self") => 1,
        Some(segment) if exports.contains(&core_root_self_alias_key(segment)) => 1,
        Some(segment) if exports.contains(segment) => 0,
        _ => return None,
    };
    if segments
        .get(start)
        .is_some_and(|segment| exports.contains(&core_root_self_alias_key(segment)))
    {
        start += 1;
    }
    if segments.len() != start + 1 {
        return None;
    }
    let mut module_name = segments[start].clone();
    let mut visited = HashSet::new();
    while let Some(source) = aliases.get(&module_name) {
        if !visited.insert(module_name.clone()) {
            return None;
        }
        module_name = source.clone();
    }
    Some(module_name)
}

fn module_public_glob_exports(
    module: &syn::ItemMod,
    source_file: &Path,
    module_dir: &Path,
    inside_inline_module: bool,
) -> Option<HashSet<String>> {
    if item_is_test_only(&module.attrs)
        || module.attrs.iter().any(|attribute| {
            !attribute_is_inspected(attribute, &HashSet::new(), 0, &HashSet::new())
        })
    {
        return None;
    }
    if let Some((_, items)) = &module.content {
        return core_items_glob_exports(items);
    }
    let resolved = resolve_module(source_file, module_dir, module, inside_inline_module)?;
    let source = fs::read_to_string(resolved.file_path).ok()?;
    let file = syn::parse_file(&source).ok()?;
    core_items_glob_exports(&file.items)
}

fn module_core_glob_exports(
    module: &syn::ItemMod,
    source_file: &Path,
    module_dir: &Path,
    inside_inline_module: bool,
    module_depth: usize,
    crate_root_core_exports: &HashSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> Option<HashSet<String>> {
    if item_is_test_only(&module.attrs)
        || module.attrs.iter().any(|attribute| {
            !attribute_is_inspected(
                attribute,
                &HashSet::new(),
                module_depth - 1,
                &HashSet::new(),
            )
        })
    {
        return None;
    }
    if let Some((_, items)) = &module.content {
        return core_items_core_glob_exports(
            items,
            source_file,
            &module_dir.join(ident_name(&module.ident)),
            true,
            module_depth,
            crate_root_core_exports,
            visited,
        );
    }
    let resolved = resolve_module(source_file, module_dir, module, inside_inline_module)?;
    let normalized = resolved
        .file_path
        .canonicalize()
        .unwrap_or_else(|_| resolved.file_path.clone());
    if !visited.insert(normalized.clone()) {
        return None;
    }
    let result = fs::read_to_string(&resolved.file_path)
        .ok()
        .and_then(|source| syn::parse_file(&source).ok())
        .and_then(|file| {
            core_items_core_glob_exports(
                &file.items,
                &resolved.file_path,
                &resolved.module_dir,
                false,
                module_depth,
                crate_root_core_exports,
                visited,
            )
        });
    visited.remove(&normalized);
    result
}

fn core_items_core_glob_exports(
    items: &[Item],
    source_file: &Path,
    module_dir: &Path,
    inside_inline_module: bool,
    module_depth: usize,
    crate_root_core_exports: &HashSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> Option<HashSet<String>> {
    let mut macro_import_violations = Vec::new();
    let inspected_macros =
        inspected_macro_imports(items, source_file, &mut macro_import_violations);
    if !macro_import_violations.is_empty() {
        return None;
    }

    let mut exports = HashSet::new();
    for item in items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if matches!(item, Item::Macro(_) | Item::Verbatim(_)) {
            return None;
        }

        if let Item::Use(item_use) = item {
            if !visibility_reaches_core_parent(&item_use.vis) {
                continue;
            }
            if item_use.attrs.iter().any(|attribute| {
                !attribute_is_inspected(attribute, &inspected_macros, module_depth, &HashSet::new())
            }) {
                return None;
            }
            collect_core_use_exports(
                &item_use.tree,
                &mut Vec::new(),
                module_depth,
                crate_root_core_exports,
                &mut exports,
            )?;
            continue;
        }

        let Some(visibility) = item_visibility(item) else {
            continue;
        };
        if !visibility_reaches_core_parent(visibility) {
            continue;
        }
        if item_attrs(item).iter().any(|attribute| {
            !attribute_is_inspected(attribute, &inspected_macros, module_depth, &HashSet::new())
        }) {
            return None;
        }

        let name = if let Item::Mod(module) = item {
            module_contains_core_dependency(
                module,
                source_file,
                module_dir,
                inside_inline_module,
                module_depth + 1,
                crate_root_core_exports,
                visited,
            )
            .then(|| ident_name(&module.ident))
        } else {
            let mut probe = CoreDependencyProbe {
                module_depth,
                crate_root_core_exports,
                inspected_macro_imports: &inspected_macros,
                found: false,
            };
            probe.visit_item(item);
            probe
                .found
                .then(|| root_item_name(item).map(ident_name))
                .flatten()
        };
        if let Some(name) = name {
            exports.insert(name);
        }
    }
    Some(exports)
}

fn collect_core_use_exports(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    module_depth: usize,
    crate_root_core_exports: &HashSet<String>,
    exports: &mut HashSet<String>,
) -> Option<()> {
    let add_if_core = |path: &[String], name: String, exports: &mut HashSet<String>| {
        if path_starts_with_core(path, module_depth)
            || path_starts_with_core_export(path, module_depth, crate_root_core_exports)
        {
            exports.insert(name);
        }
    };
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_name(&path.ident));
            collect_core_use_exports(
                &path.tree,
                prefix,
                module_depth,
                crate_root_core_exports,
                exports,
            )?;
            prefix.pop();
        }
        UseTree::Name(name) => {
            prefix.push(ident_name(&name.ident));
            let exported_name = if ident_name(&name.ident) == "self" {
                prefix.get(prefix.len().checked_sub(2)?)?.clone()
            } else {
                ident_name(&name.ident)
            };
            add_if_core(prefix, exported_name, exports);
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(ident_name(&rename.ident));
            let exported_name = ident_name(&rename.rename);
            add_if_core(prefix, exported_name, exports);
            prefix.pop();
        }
        UseTree::Glob(_) => {
            if path_starts_with_core(prefix, module_depth)
                || path_starts_with_core_export(prefix, module_depth, crate_root_core_exports)
            {
                return None;
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_core_use_exports(
                    item,
                    prefix,
                    module_depth,
                    crate_root_core_exports,
                    exports,
                )?;
            }
        }
    }
    Some(())
}

fn core_items_glob_exports(items: &[Item]) -> Option<HashSet<String>> {
    let mut exports = HashSet::new();
    for item in items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if matches!(item, Item::Macro(_) | Item::Verbatim(_)) {
            return None;
        }
        if item_attrs(item).iter().any(|attribute| {
            !attribute_is_inspected(attribute, &HashSet::new(), 1, &HashSet::new())
        }) {
            return None;
        }
        if let Item::Use(item_use) = item {
            if !visibility_reaches_core_parent(&item_use.vis) {
                continue;
            }
            collect_use_tree_exports(&item_use.tree, &mut Vec::new(), &mut exports)?;
            continue;
        }
        if item_visibility(item).is_some_and(visibility_reaches_core_parent) {
            let name = match item {
                Item::Mod(item) => Some(&item.ident),
                _ => root_item_name(item),
            };
            if let Some(name) = name {
                exports.insert(ident_name(name));
            }
        }
    }
    Some(exports)
}

fn item_visibility(item: &Item) -> Option<&syn::Visibility> {
    match item {
        Item::Const(item) => Some(&item.vis),
        Item::Enum(item) => Some(&item.vis),
        Item::ExternCrate(item) => Some(&item.vis),
        Item::Fn(item) => Some(&item.vis),
        Item::Mod(item) => Some(&item.vis),
        Item::Static(item) => Some(&item.vis),
        Item::Struct(item) => Some(&item.vis),
        Item::Trait(item) => Some(&item.vis),
        Item::TraitAlias(item) => Some(&item.vis),
        Item::Type(item) => Some(&item.vis),
        Item::Union(item) => Some(&item.vis),
        Item::Use(item) => Some(&item.vis),
        _ => None,
    }
}

fn visibility_reaches_core_parent(visibility: &syn::Visibility) -> bool {
    match visibility {
        syn::Visibility::Public(_) => true,
        syn::Visibility::Restricted(restricted) => {
            let segments = restricted
                .path
                .segments
                .iter()
                .map(|segment| ident_name(&segment.ident))
                .collect::<Vec<_>>();
            matches!(segments.as_slice(), [crate_name] if crate_name == "crate")
                || matches!(segments.as_slice(), [super_name] if super_name == "super")
        }
        syn::Visibility::Inherited => false,
    }
}

fn collect_use_tree_exports(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    exports: &mut HashSet<String>,
) -> Option<()> {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_name(&path.ident));
            collect_use_tree_exports(&path.tree, prefix, exports)?;
            prefix.pop();
        }
        UseTree::Name(name) => {
            let exported = if ident_name(&name.ident) == "self" {
                prefix.last()?.clone()
            } else {
                ident_name(&name.ident)
            };
            exports.insert(exported);
        }
        UseTree::Rename(rename) => {
            let exported = ident_name(&rename.rename);
            if exported != "_" {
                exports.insert(exported);
            }
        }
        UseTree::Glob(_) => return None,
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_tree_exports(item, prefix, exports)?;
            }
        }
    }
    Some(())
}

fn root_path_starts_with_core_or_export(segments: &[String], exports: &HashSet<String>) -> bool {
    let mut start = match segments.first().map(String::as_str) {
        Some("crate" | "$crate" | "self") => 1,
        Some("super") => return false,
        Some(segment) if exports.contains(segment) => 0,
        Some(segment) if exports.contains(&core_root_self_alias_key(segment)) => 1,
        Some(_) | None => return false,
    };
    if segments.get(start).is_some_and(|segment| segment == "self") {
        start += 1;
    }
    if segments
        .get(start)
        .is_some_and(|segment| exports.contains(&core_root_self_alias_key(segment)))
    {
        start += 1;
    }
    segments.get(start).is_some_and(|segment| {
        matches!(segment.as_str(), "core" | "task_lifecycle") || exports.contains(segment)
    })
}

fn path_starts_with_core(segments: &[String], module_depth: usize) -> bool {
    if matches!(
        segments.first().map(String::as_str),
        Some("crate" | "$crate")
    ) {
        let next = usize::from(segments.get(1).is_some_and(|segment| segment == "self")) + 1;
        return segments
            .get(next)
            .is_some_and(|segment| matches!(segment.as_str(), "core" | "task_lifecycle"));
    }
    let start = usize::from(segments.first().is_some_and(|segment| segment == "self"));
    let super_count = segments[start..]
        .iter()
        .take_while(|segment| segment.as_str() == "super")
        .count();
    super_count == module_depth
        && segments
            .get(start + super_count)
            .is_some_and(|segment| matches!(segment.as_str(), "core" | "task_lifecycle"))
}

fn path_starts_with_core_export(
    segments: &[String],
    module_depth: usize,
    exports: &HashSet<String>,
) -> bool {
    if segments.len() > 1
        && segments
            .first()
            .is_some_and(|segment| exports.contains(segment))
    {
        return true;
    }
    let mut start = match segments.first().map(String::as_str) {
        Some("crate" | "$crate" | "self") => 1,
        Some("super") => 0,
        Some(segment) if exports.contains(&core_root_self_alias_key(segment)) => 1,
        Some(_) => return false,
        None => return false,
    };
    if segments.get(start).is_some_and(|segment| segment == "self") {
        start += 1;
    }
    let super_count = segments[start..]
        .iter()
        .take_while(|segment| segment.as_str() == "super")
        .count();
    if super_count != 0 {
        if super_count != module_depth {
            return false;
        }
        start += super_count;
    } else if start == 0
        || (segments.first().is_some_and(|segment| segment == "self") && module_depth != 0)
    {
        return false;
    }
    if segments
        .get(start)
        .is_some_and(|segment| exports.contains(&core_root_self_alias_key(segment)))
    {
        start += 1;
    }
    segments.get(start).is_some_and(|segment| {
        matches!(segment.as_str(), "core" | "task_lifecycle") || exports.contains(segment)
    })
}

fn core_root_self_alias_key(alias: &str) -> String {
    format!("{CORE_ROOT_SELF_ALIAS_PREFIX}{alias}>")
}

fn core_root_module_alias_key(alias: &str) -> String {
    format!("{CORE_ROOT_MODULE_ALIAS_PREFIX}{alias}>")
}

fn use_tree_starts_with_core_export(
    segments: &[String],
    module_depth: usize,
    exports: &HashSet<String>,
) -> bool {
    path_starts_with_core_export(segments, module_depth, exports)
        || segments.len() == 1
            && segments
                .first()
                .is_some_and(|segment| exports.contains(segment))
}

fn inspect_core_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    module_depth: usize,
    exports: &HashSet<String>,
    origin: &str,
    violations: &mut Vec<String>,
) {
    let record = |prefix: &[String], violations: &mut Vec<String>| {
        if path_starts_with_core(prefix, module_depth)
            || use_tree_starts_with_core_export(prefix, module_depth, exports)
        {
            violations.push(format!("{origin}: production Tools import depends on Core"));
        }
    };
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_name(&path.ident));
            inspect_core_use_tree(
                &path.tree,
                prefix,
                module_depth,
                exports,
                origin,
                violations,
            );
            prefix.pop();
        }
        UseTree::Name(name) => {
            prefix.push(ident_name(&name.ident));
            record(prefix, violations);
            prefix.pop();
        }
        UseTree::Rename(rename) => {
            prefix.push(ident_name(&rename.ident));
            record(prefix, violations);
            if path_resolves_to_crate_root(prefix, module_depth) {
                violations.push(format!(
                    "{origin}: production Tools crate-root alias `{}` is not inspected",
                    ident_name(&rename.rename)
                ));
            }
            prefix.pop();
        }
        UseTree::Glob(_) => {
            record(prefix, violations);
            if path_resolves_to_crate_root(prefix, module_depth) {
                violations.push(format!(
                    "{origin}: production Tools crate-root glob is not inspected"
                ));
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                inspect_core_use_tree(item, prefix, module_depth, exports, origin, violations);
            }
        }
    }
}

fn macro_tokens_contain_core_path(
    tokens: &TokenStream,
    module_depth: usize,
    exports: &HashSet<String>,
) -> bool {
    let tokens: Vec<_> = tokens.clone().into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        if let TokenTree::Group(group) = token {
            if macro_tokens_contain_core_path(&group.stream(), module_depth, exports) {
                return true;
            }
        }
        let Some((segments, end)) = token_path_at(&tokens, index) else {
            continue;
        };
        if path_starts_with_core(&segments, module_depth)
            || path_starts_with_core_export(&segments, module_depth, exports)
            || path_resolves_to_crate_root(&segments, module_depth)
                && (tokens.get(end).is_some_and(
                    |token| matches!(token, TokenTree::Ident(ident) if ident_name(ident) == "as"),
                ) || token_path_is_grouped(&tokens, end)
                    || tokens.get(end).is_some_and(
                        |token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '*'),
                    ))
        {
            return true;
        }
    }
    false
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
    let mut macro_import_violations = Vec::new();
    let root_inspected_macro_imports =
        inspected_macro_imports(&file.items, root, &mut macro_import_violations);
    violations.extend(macro_import_violations);
    let root_item_names = file
        .items
        .iter()
        .filter_map(root_item_name)
        .map(ident_name)
        .collect::<HashSet<_>>();
    for item in &file.items {
        if item_is_test_only(item_attrs(item)) {
            continue;
        }
        if matches!(item, Item::Verbatim(_)) {
            violations.push(format!(
                "{}: uninspected production crate-root Rust syntax",
                root.display()
            ));
            continue;
        }
        if item_attrs(item)
            .iter()
            .any(|attribute| path_is_ident(attribute.path(), "macro_use"))
        {
            violations.push(format!(
                "{}: crate-root macro imports are not inspected",
                root.display()
            ));
        }
        let root_item_attributes = item_attrs(item);
        for attribute in root_item_attributes {
            if !attribute_is_inspected(
                attribute,
                &root_inspected_macro_imports,
                0,
                &crate_root_brain_exports,
            ) {
                violations.push(format!(
                    "{}: uninspected production crate-root item attribute `{}`",
                    root.display(),
                    path_label(attribute.path())
                ));
            }
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
            Item::Mod(_) => {}
            Item::ExternCrate(item)
                if ident_name(&item.ident) == "self" && item.rename.is_some() =>
            {
                violations.push(format!(
                    "{}: crate-root extern alias `{}` is not checked",
                    root.display(),
                    item.rename
                        .as_ref()
                        .map(|(_, ident)| ident_name(ident))
                        .unwrap_or_default()
                ));
            }
            Item::ExternCrate(item)
                if item
                    .attrs
                    .iter()
                    .any(|attribute| path_is_ident(attribute.path(), "macro_use")) =>
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

    // Root modules can re-export Brain types below the crate root, including
    // through another nested facade. Treat each module that contains a
    // Brain-backed import as a blocked root prefix, then repeat so re-exports
    // through previously discovered facades are also covered.
    loop {
        let mut added_export = false;
        for item in &file.items {
            let Item::Mod(module) = item else {
                continue;
            };
            if ident_name(&module.ident) == "tools" || item_is_test_only(&module.attrs) {
                continue;
            }
            if module_contains_brain_import(
                module,
                root,
                &module_directory(root),
                false,
                1,
                &crate_root_brain_exports,
                &mut HashSet::new(),
            ) && crate_root_brain_exports.insert(ident_name(&module.ident))
            {
                added_export = true;
            }
        }

        for item in &file.items {
            let Item::Use(item) = item else {
                continue;
            };
            if item_is_test_only(&item.attrs) {
                continue;
            }
            let previous_count = crate_root_brain_exports.len();
            let mut ignored_violations = Vec::new();
            inspect_crate_root_alias(
                &item.tree,
                &mut Vec::new(),
                &root.display().to_string(),
                &mut ignored_violations,
                &mut crate_root_brain_exports,
            );
            added_export |= crate_root_brain_exports.len() != previous_count;
        }

        for item in &file.items {
            if item_is_test_only(item_attrs(item))
                || matches!(
                    item,
                    Item::Use(_) | Item::Mod(_) | Item::ExternCrate(_) | Item::Macro(_)
                )
            {
                continue;
            }
            match item {
                Item::Impl(implementation)
                    if item_contains_brain_import(
                        item,
                        0,
                        &crate_root_brain_exports,
                        &root_inspected_macro_imports,
                    ) =>
                {
                    if let Some(name) = root_impl_target_name(implementation) {
                        if root_item_names.contains(&name) {
                            added_export |= crate_root_brain_exports.insert(name);
                        }
                    }
                }
                Item::ForeignMod(foreign_mod) => {
                    for foreign_item in &foreign_mod.items {
                        if matches!(
                            foreign_item,
                            syn::ForeignItem::Macro(_) | syn::ForeignItem::Verbatim(_)
                        ) {
                            violations.push(format!(
                                "{}: crate-root foreign macro or syntax is not inspected",
                                root.display()
                            ));
                            continue;
                        }
                        if item_is_test_only(foreign_item_attrs(foreign_item))
                            || !foreign_item_contains_brain_import(
                                foreign_item,
                                &crate_root_brain_exports,
                                &root_inspected_macro_imports,
                            )
                        {
                            continue;
                        }
                        if let Some(name) = foreign_item_name(foreign_item) {
                            added_export |= crate_root_brain_exports.insert(ident_name(name));
                        }
                    }
                }
                _ => {
                    let Some(name) = root_item_name(item) else {
                        continue;
                    };
                    if item_contains_brain_import(
                        item,
                        0,
                        &crate_root_brain_exports,
                        &root_inspected_macro_imports,
                    ) {
                        added_export |= crate_root_brain_exports.insert(ident_name(name));
                    }
                }
            }
        }

        if !added_export {
            break;
        }
    }
    crate_root_brain_exports
}

fn module_contains_brain_import(
    module: &syn::ItemMod,
    source_file: &Path,
    module_dir: &Path,
    inside_inline_module: bool,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> bool {
    if item_is_test_only(&module.attrs) {
        return false;
    }
    if module.attrs.iter().any(|attribute| {
        !attribute_is_inspected(
            attribute,
            &HashSet::new(),
            module_depth,
            crate_root_brain_exports,
        )
    }) {
        return true;
    }
    if let Some((_, items)) = &module.content {
        if module_has_path_attribute(&module.attrs) {
            return true;
        }
        return items_contain_brain_import(
            items,
            source_file,
            &module_dir.join(ident_name(&module.ident)),
            true,
            module_depth,
            crate_root_brain_exports,
            visited,
        );
    }

    let Some(resolved_module) =
        resolve_module(source_file, module_dir, module, inside_inline_module)
    else {
        return true;
    };
    let normalized = resolved_module
        .file_path
        .canonicalize()
        .unwrap_or_else(|_| resolved_module.file_path.clone());
    if !visited.insert(normalized.clone()) {
        return true;
    }
    let source = fs::read_to_string(&resolved_module.file_path);
    let result = match source.and_then(|source| {
        syn::parse_file(&source)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }) {
        Ok(file) => items_contain_brain_import(
            &file.items,
            &resolved_module.file_path,
            &resolved_module.module_dir,
            false,
            module_depth,
            crate_root_brain_exports,
            visited,
        ),
        Err(_) => true,
    };
    visited.remove(&normalized);
    result
}

fn items_contain_brain_import(
    items: &[Item],
    source_file: &Path,
    module_dir: &Path,
    inside_inline_module: bool,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> bool {
    let mut macro_import_violations = Vec::new();
    let inspected_macros =
        inspected_macro_imports(items, source_file, &mut macro_import_violations);
    if !macro_import_violations.is_empty() {
        return true;
    }
    items.iter().any(|item| {
        if item_is_test_only(item_attrs(item)) {
            return false;
        }
        match item {
            Item::Mod(nested) => module_contains_brain_import(
                nested,
                source_file,
                module_dir,
                inside_inline_module,
                module_depth + 1,
                crate_root_brain_exports,
                visited,
            ),
            // A macro can emit an import or public facade that is absent from
            // this AST. Mark the containing root module as uninspectable.
            Item::Macro(_) => true,
            _ => item_contains_brain_import(
                item,
                module_depth,
                crate_root_brain_exports,
                &inspected_macros,
            ),
        }
    })
}

fn item_contains_brain_import(
    item: &Item,
    module_depth: usize,
    crate_root_brain_exports: &HashSet<String>,
    inspected_macro_imports: &HashSet<String>,
) -> bool {
    let mut violations = Vec::new();
    let mut visitor = DirectionVisitor {
        origin: "facade item".to_owned(),
        module_depth,
        crate_root_brain_exports,
        crate_root_core_exports: None,
        inspected_macro_imports,
        violations: &mut violations,
    };
    visitor.visit_item(item);
    if !violations.is_empty() {
        return true;
    }
    let mut root_facade_probe = RootFacadePathProbe {
        exports: crate_root_brain_exports,
        found: false,
    };
    root_facade_probe.visit_item(item);
    root_facade_probe.found
}

fn foreign_item_contains_brain_import(
    item: &syn::ForeignItem,
    crate_root_brain_exports: &HashSet<String>,
    inspected_macro_imports: &HashSet<String>,
) -> bool {
    let mut violations = Vec::new();
    let mut visitor = DirectionVisitor {
        origin: "crate-root foreign item".to_owned(),
        module_depth: 0,
        crate_root_brain_exports,
        crate_root_core_exports: None,
        inspected_macro_imports,
        violations: &mut violations,
    };
    visitor.visit_foreign_item(item);
    if !violations.is_empty() {
        return true;
    }
    let mut root_facade_probe = RootFacadePathProbe {
        exports: crate_root_brain_exports,
        found: false,
    };
    root_facade_probe.visit_foreign_item(item);
    root_facade_probe.found
}

fn foreign_item_name(item: &syn::ForeignItem) -> Option<&Ident> {
    match item {
        syn::ForeignItem::Fn(item) => Some(&item.sig.ident),
        syn::ForeignItem::Static(item) => Some(&item.ident),
        syn::ForeignItem::Type(item) => Some(&item.ident),
        _ => None,
    }
}

fn foreign_item_attrs(item: &syn::ForeignItem) -> &[Attribute] {
    match item {
        syn::ForeignItem::Fn(item) => &item.attrs,
        syn::ForeignItem::Static(item) => &item.attrs,
        syn::ForeignItem::Type(item) => &item.attrs,
        syn::ForeignItem::Macro(item) => &item.attrs,
        syn::ForeignItem::Verbatim(_) => &[],
        _ => &[],
    }
}

fn root_impl_target_name(implementation: &syn::ItemImpl) -> Option<String> {
    let syn::Type::Path(type_path) = implementation.self_ty.as_ref() else {
        return None;
    };
    type_path
        .path
        .segments
        .last()
        .map(|segment| ident_name(&segment.ident))
}

struct RootFacadePathProbe<'a> {
    exports: &'a HashSet<String>,
    found: bool,
}

impl<'ast> Visit<'ast> for RootFacadePathProbe<'_> {
    fn visit_path(&mut self, path: &'ast RustPath) {
        if path.leading_colon.is_none()
            && path
                .segments
                .first()
                .is_some_and(|segment| self.exports.contains(&ident_name(&segment.ident)))
        {
            self.found = true;
        }
        syn::visit::visit_path(self, path);
    }
}

fn root_item_name(item: &Item) -> Option<&Ident> {
    match item {
        Item::Const(item) => Some(&item.ident),
        Item::Enum(item) => Some(&item.ident),
        Item::Fn(item) => Some(&item.sig.ident),
        Item::Static(item) => Some(&item.ident),
        Item::Struct(item) => Some(&item.ident),
        Item::Trait(item) => Some(&item.ident),
        Item::TraitAlias(item) => Some(&item.ident),
        Item::Type(item) => Some(&item.ident),
        Item::Union(item) => Some(&item.ident),
        _ => None,
    }
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
            prefix.push(ident_name(&path.ident));
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
            prefix.push(ident_name(&rename.ident));
            if root_path_starts_with_brain_or_export(prefix, crate_root_brain_exports) {
                crate_root_brain_exports.insert(ident_name(&rename.rename));
            }
            if path_resolves_to_crate_root(prefix, 0) {
                violations.push(format!(
                    "{origin}: crate-root import alias `{}` is not checked",
                    ident_name(&rename.rename)
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
            prefix.push(ident_name(&name.ident));
            if root_path_starts_with_brain_or_export(prefix, crate_root_brain_exports) {
                crate_root_brain_exports.insert(ident_name(&name.ident));
            }
            prefix.pop();
        }
        UseTree::Glob(_) => {
            if root_path_starts_with_brain_or_export(prefix, crate_root_brain_exports) {
                violations.push(format!(
                    "{origin}: crate-root Brain glob import is not inspected"
                ));
            }
        }
    }
}

fn root_path_starts_with_brain_or_export(
    segments: &[String],
    crate_root_brain_exports: &HashSet<String>,
) -> bool {
    if root_path_starts_with_brain(segments) {
        return true;
    }
    let root_segment = match segments.first().map(String::as_str) {
        Some("crate" | "$crate" | "self") => segments.get(1).map(String::as_str),
        Some("super") => None,
        Some(segment) => Some(segment),
        None => None,
    };
    root_segment.is_some_and(|segment| crate_root_brain_exports.contains(segment))
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
        "allow" | "cfg" | "doc" | "ignore" | "link" | "test" => true,
        "path" => match &attribute.meta {
            Meta::NameValue(name_value) => {
                matches!(&name_value.value, syn::Expr::Lit(expression) if matches!(&expression.lit, syn::Lit::Str(_)))
            }
            _ => false,
        },
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

fn serde_attribute_contains_core_path(
    attribute: &Attribute,
    module_depth: usize,
    crate_root_core_exports: &HashSet<String>,
) -> bool {
    if !path_is_ident(attribute.path(), "serde") {
        return false;
    }
    let parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
    attribute
        .parse_args_with(parser)
        .map(|items| {
            items.iter().any(|item| {
                serde_meta_contains_core_path(item, module_depth, crate_root_core_exports)
            })
        })
        .unwrap_or(true)
}

fn serde_meta_contains_core_path(
    meta: &Meta,
    module_depth: usize,
    crate_root_core_exports: &HashSet<String>,
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
                    syn::Lit::Str(value) => serde_string_contains_core_path(
                        &value.value(),
                        module_depth,
                        crate_root_core_exports,
                    ),
                    _ => false,
                },
                expression => {
                    let mut probe = CoreDependencyProbe {
                        module_depth,
                        crate_root_core_exports,
                        inspected_macro_imports: &HashSet::new(),
                        found: false,
                    };
                    probe.visit_expr(expression);
                    probe.found
                }
            }
        }
        Meta::List(list) => {
            let parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
            match syn::parse::Parser::parse2(parser, list.tokens.clone()) {
                Ok(items) => items.iter().any(|item| {
                    serde_meta_contains_core_path(item, module_depth, crate_root_core_exports)
                }),
                Err(_) => true,
            }
        }
    }
}

fn serde_string_contains_core_path(
    value: &str,
    module_depth: usize,
    crate_root_core_exports: &HashSet<String>,
) -> bool {
    if let Ok(path) = syn::parse_str::<RustPath>(value) {
        let segments = path
            .segments
            .iter()
            .map(|segment| ident_name(&segment.ident))
            .collect::<Vec<_>>();
        if path_starts_with_core(&segments, module_depth)
            || path_starts_with_core_export(&segments, module_depth, crate_root_core_exports)
        {
            return true;
        }
    }

    if let Ok(ty) = syn::parse_str::<syn::Type>(value) {
        let mut probe = CoreDependencyProbe {
            module_depth,
            crate_root_core_exports,
            inspected_macro_imports: &HashSet::new(),
            found: false,
        };
        probe.visit_type(&ty);
        if probe.found {
            return true;
        }
    }

    let parser =
        syn::punctuated::Punctuated::<syn::WherePredicate, syn::Token![,]>::parse_terminated;
    if let Ok(predicates) = syn::parse::Parser::parse_str(parser, value) {
        return predicates.iter().any(|predicate| {
            let mut probe = CoreDependencyProbe {
                module_depth,
                crate_root_core_exports,
                inspected_macro_imports: &HashSet::new(),
                found: false,
            };
            probe.visit_where_predicate(predicate);
            probe.found
        });
    }
    false
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
    if let Ok(path) = syn::parse_str::<RustPath>(value) {
        let segments = path
            .segments
            .iter()
            .map(|segment| ident_name(&segment.ident))
            .collect::<Vec<_>>();
        if path_starts_with_brain(&segments, module_depth)
            || path_starts_with_brain_export(&segments, module_depth, crate_root_brain_exports)
        {
            return true;
        }
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
            .map(|segment| ident_name(&segment.ident))
            .collect::<Vec<_>>();
        self.found |= path_starts_with_brain(&segments, self.module_depth)
            || path_starts_with_brain_export(
                &segments,
                self.module_depth,
                self.crate_root_brain_exports,
            )
            || self.module_depth == 0
                && segments
                    .first()
                    .is_some_and(|segment| self.crate_root_brain_exports.contains(segment));
        syn::visit::visit_path(self, path);
    }
}

fn derive_is_inspected(derive: &RustPath, imported: &HashSet<String>) -> bool {
    let segments: Vec<_> = derive
        .segments
        .iter()
        .map(|segment| ident_name(&segment.ident))
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
        && ident_name(&path.segments[0].ident) == "serde_json"
        && ident_name(&path.segments[1].ident) == "json"
    {
        return true;
    }
    path.segments.len() == 1
        && (PRELUDE_MACROS.contains(&ident_name(&path.segments[0].ident).as_str())
            || imported.contains(&ident_name(&path.segments[0].ident)))
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
                    .any(|attribute| path_is_ident(attribute.path(), "macro_use")) =>
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
            prefix.push(ident_name(&path.ident));
            inspect_macro_import_tree(&path.tree, prefix, origin, imported, violations);
            prefix.pop();
        }
        UseTree::Name(name) => {
            if ident_name(&name.ident) == "self" {
                if let Some(local_name) = prefix.last() {
                    inspect_macro_import(prefix, local_name.clone(), origin, imported, violations);
                }
            } else {
                prefix.push(ident_name(&name.ident));
                inspect_macro_import(
                    prefix,
                    ident_name(&name.ident),
                    origin,
                    imported,
                    violations,
                );
                prefix.pop();
            }
        }
        UseTree::Rename(rename) => {
            if ident_name(&rename.ident) == "self" {
                inspect_macro_import(
                    prefix,
                    ident_name(&rename.rename),
                    origin,
                    imported,
                    violations,
                );
            } else {
                prefix.push(ident_name(&rename.ident));
                inspect_macro_import(
                    prefix,
                    ident_name(&rename.rename),
                    origin,
                    imported,
                    violations,
                );
                prefix.pop();
            }
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
        || BUILTIN_DERIVES.contains(&local_name.as_str())
        || SERDE_DERIVES.contains(&local_name.as_str())
        || is_inspected_crate_root_name(&local_name)
        || local_name == "json";
    if !could_shadow_inspected {
        return;
    }
    let trusted = source == ["serde_json", "json"] && local_name == "json"
        || source.len() == 2
            && matches!(source[0].as_str(), "serde" | "serde_derive")
            && SERDE_DERIVES.contains(&source[1].as_str())
            && source[1] == local_name
        || source.len() == 1
            && source[0] == local_name
            && is_inspected_crate_root_name(&local_name)
        || source.len() == 2
            && matches!(source[0].as_str(), "std" | "core")
            && source[1] == local_name
            && PRELUDE_MACROS.contains(&local_name.as_str())
        || standard_builtin_derive_trait_import(source, &local_name);
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

fn standard_builtin_derive_trait_import(source: &[String], local_name: &str) -> bool {
    if source.len() != 3 || !matches!(source[0].as_str(), "std" | "core") || source[2] != local_name
    {
        return false;
    }
    match local_name {
        "Clone" => source[1] == "clone",
        "Copy" => source[1] == "marker",
        "Debug" => source[1] == "fmt",
        "Default" => source[1] == "default",
        "Eq" | "Ord" | "PartialEq" | "PartialOrd" => source[1] == "cmp",
        "Hash" => source[1] == "hash",
        _ => false,
    }
}

fn path_label(path: &RustPath) -> String {
    path.segments
        .iter()
        .map(|segment| ident_name(&segment.ident))
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
                && (tokens.get(end).is_some_and(
                    |token| matches!(token, TokenTree::Ident(ident) if ident_name(ident) == "as"),
                ) || token_path_is_grouped(&tokens, end))
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

fn token_path_is_grouped(tokens: &[TokenTree], end: usize) -> bool {
    matches!(tokens.get(end), Some(TokenTree::Punct(punct)) if punct.as_char() == ':')
        && matches!(tokens.get(end + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == ':')
        && matches!(tokens.get(end + 2), Some(TokenTree::Group(_)))
}

fn token_path_at(tokens: &[TokenTree], start: usize) -> Option<(Vec<String>, usize)> {
    let mut index = start;
    let mut segments = Vec::new();
    if matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '$') {
        let TokenTree::Ident(ident) = tokens.get(index + 1)? else {
            return None;
        };
        if ident_name(ident) != "crate" {
            return None;
        }
        segments.push("$crate".to_owned());
        index += 2;
    } else if let Some(TokenTree::Ident(ident)) = tokens.get(index) {
        segments.push(ident_name(ident));
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
        segments.push(ident_name(ident));
        index += 3;
    }
    Some((segments, index))
}
