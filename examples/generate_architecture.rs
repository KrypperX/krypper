//! Deterministic README architecture generation from existing source declarations.
use std::{env, fs, path::Path};

const START: &str = "<!-- architecture:generated:start -->";
const END: &str = "<!-- architecture:generated:end -->";

fn module_names(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("pub(crate) ").unwrap_or(line);
            let rest = line
                .strip_prefix("pub mod ")
                .or_else(|| line.strip_prefix("mod "))?;
            let name = rest.strip_suffix(';')?;
            if name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                Some(name.to_owned())
            } else {
                None
            }
        })
        .collect()
}

fn operation_names(source: &str) -> Result<Vec<String>, String> {
    let section = source
        .split("impl WorkspaceOperation {")
        .nth(1)
        .ok_or("WorkspaceOperation implementation missing")?;
    let mapping = section
        .split("const fn name(")
        .nth(1)
        .ok_or("WorkspaceOperation name mapping missing")?;
    let mapping = mapping
        .split("\n}\n")
        .next()
        .ok_or("name mapping incomplete")?;
    let result: Vec<String> = mapping
        .lines()
        .filter_map(|line| {
            let (_, value) = line.trim().split_once(" => \"")?;
            let name = value.strip_suffix("\",")?;
            if name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                Some(name.to_owned())
            } else {
                None
            }
        })
        .collect();
    if result.is_empty() {
        return Err("No workspace operations found".into());
    }
    Ok(result)
}

fn diagram(modules: &[String], operations: &[String]) -> String {
    let mut out = String::from("```mermaid\nflowchart TD\n");
    for name in modules {
        out.push_str(&format!(
            "    root[\"Krypper crate\"] --> m_{name}[\"{name}\"]\n"
        ));
    }
    out.push_str("    m_tools --> registry[\"Typed workspace registry\"]\n");
    for name in operations {
        out.push_str(&format!("    registry --> op_{name}[\"{name}\"]\n"));
    }
    out.push_str("```\n");
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let check = match env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [arg] if arg == "--check" => true,
        _ => return Err("usage: generate_architecture [--check]".into()),
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let modules = module_names(&fs::read_to_string(root.join("src/lib.rs"))?);
    if !modules.iter().any(|name| name == "tools") {
        return Err("tools module missing".into());
    }
    let operations = operation_names(&fs::read_to_string(root.join("src/tools/registry.rs"))?)?;
    let path = root.join("README.md");
    let original = fs::read_to_string(&path)?;
    if original.matches(START).count() != 1 || original.matches(END).count() != 1 {
        return Err("README must contain exactly one generated architecture section".into());
    }
    let (before, rest) = original.split_once(START).ok_or("missing start marker")?;
    let (_, after) = rest.split_once(END).ok_or("missing end marker")?;
    let expected = format!(
        "{before}{START}\n{}{END}{after}",
        diagram(&modules, &operations)
    );
    if check {
        if original != expected {
            return Err(
                "README architecture is stale; run cargo run --example generate_architecture"
                    .into(),
            );
        }
        println!("README architecture is current");
    } else if original != expected {
        fs::write(path, expected)?;
        println!("Updated README architecture");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_declared_modules() {
        assert_eq!(
            module_names("pub mod cli;\nmod brain;\n// mod fake;\n"),
            vec!["cli", "brain"]
        );
    }
    #[test]
    fn rejects_missing_registry_mapping() {
        assert!(operation_names("enum WorkspaceOperation {}").is_err());
    }
}
