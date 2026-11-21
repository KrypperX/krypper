use super::{
    load_skills, SkillLoadError, MAX_SKILL_MANIFESTS, MAX_SKILL_MANIFEST_BYTES,
    SKILL_MANIFEST_SCHEMA,
};
use crate::tools::{capture_workspace_root_at, registry, WorkspaceRoot};
use serde_json::json;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    path: PathBuf,
    root: WorkspaceRoot,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "krypper-skills-{}-{}-{}",
            label,
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let root = capture_workspace_root_at(&path).unwrap();
        Self { path, root }
    }

    fn write(&self, relative: &str, contents: &str) {
        let target = self.path.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, contents).unwrap();
    }

    fn manifest(&self, file: &str, name: &str) {
        self.write(file, &manifest_json(name));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn manifest_json(name: &str) -> String {
    json!({
        "schema": SKILL_MANIFEST_SCHEMA,
        "name": name,
        "description": "short description",
        "instructions": format!("instructions for {name}"),
    })
    .to_string()
}

#[test]
fn loads_project_manifests_in_deterministic_name_order() {
    let fixture = Fixture::new("ordering");
    fixture.manifest(".krypper/skills/zeta.json", "zeta");
    fixture.manifest(".krypper/skills/alpha.json", "alpha");

    let first = load_skills(&fixture.root, &[]).unwrap();
    let second = load_skills(&fixture.root, &[]).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        first
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(first[0].instructions.as_str(), "instructions for alpha");
    assert_eq!(first[0].description, "short description");
    assert_eq!(
        first[0].manifest_path,
        PathBuf::from(".krypper/skills/alpha.json")
    );
}

#[test]
fn loads_explicit_paths_and_ignores_non_manifest_files() {
    let fixture = Fixture::new("explicit");
    fixture.manifest("custom/explicit.json", "explicit");
    fixture.write(".krypper/skills/notes.txt", "not a manifest");

    let skills = load_skills(&fixture.root, &[PathBuf::from("custom/explicit.json")]).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "explicit");
    assert_eq!(
        skills[0].manifest_path,
        PathBuf::from("custom/explicit.json")
    );
}

#[test]
fn missing_project_directory_loads_nothing() {
    let fixture = Fixture::new("missing");
    assert!(load_skills(&fixture.root, &[]).unwrap().is_empty());
}

#[test]
fn rejects_missing_explicit_manifest() {
    let fixture = Fixture::new("not-found");
    assert_eq!(
        load_skills(&fixture.root, &[PathBuf::from("custom/missing.json")]),
        Err(SkillLoadError::NotFound)
    );
}

#[test]
fn rejects_absolute_and_parent_paths() {
    let fixture = Fixture::new("invalid-paths");
    assert_eq!(
        load_skills(&fixture.root, &[PathBuf::from("/etc/hostname")]),
        Err(SkillLoadError::InvalidPath)
    );
    assert_eq!(
        load_skills(&fixture.root, &[PathBuf::from("../outside.json")]),
        Err(SkillLoadError::OutsideWorkspace)
    );
    let deep = std::iter::repeat("deep")
        .take(17)
        .collect::<Vec<_>>()
        .join("/");
    assert_eq!(
        load_skills(&fixture.root, &[PathBuf::from(format!("{deep}.json"))]),
        Err(SkillLoadError::InvalidPath)
    );
}

#[test]
fn rejects_symlinked_manifest() {
    let fixture = Fixture::new("symlink-file");
    fixture.manifest(".krypper/skills/alpha.json", "alpha");
    symlink(
        fixture.path.join(".krypper/skills/alpha.json"),
        fixture.path.join("alias.json"),
    )
    .unwrap();

    assert_eq!(
        load_skills(&fixture.root, &[PathBuf::from("alias.json")]),
        Err(SkillLoadError::Symlink)
    );
    assert_eq!(load_skills(&fixture.root, &[]).unwrap()[0].name, "alpha");
}

#[test]
fn rejects_symlinked_discovery_directory() {
    let fixture = Fixture::new("symlink-dir");
    fixture.manifest("real/alpha.json", "alpha");
    fs::create_dir(fixture.path.join(".krypper")).unwrap();
    symlink(
        fixture.path.join("real"),
        fixture.path.join(".krypper/skills"),
    )
    .unwrap();

    assert_eq!(
        load_skills(&fixture.root, &[]),
        Err(SkillLoadError::Symlink)
    );
}

#[test]
fn rejects_symlink_that_escapes_the_workspace() {
    let fixture = Fixture::new("symlink-escape");
    let outside = fixture.path.with_extension("outside.json");
    fs::write(&outside, manifest_json("alpha")).unwrap();
    symlink(&outside, fixture.path.join("escape.json")).unwrap();

    assert_eq!(
        load_skills(&fixture.root, &[PathBuf::from("escape.json")]),
        Err(SkillLoadError::Symlink)
    );
    fs::remove_file(outside).unwrap();
}

#[test]
fn rejects_oversize_manifest() {
    let fixture = Fixture::new("oversize");
    let manifest = json!({
        "schema": SKILL_MANIFEST_SCHEMA,
        "name": "alpha",
        "description": "description",
        "instructions": "x".repeat(MAX_SKILL_MANIFEST_BYTES),
    })
    .to_string();
    fixture.write(".krypper/skills/alpha.json", &manifest);

    assert_eq!(
        load_skills(&fixture.root, &[]),
        Err(SkillLoadError::TooLarge)
    );
}

#[test]
fn rejects_invalid_manifests() {
    let fixture = Fixture::new("invalid");
    let cases = [
        ("malformed", "{".to_owned()),
        (
            "wrong-schema",
            json!({
                "schema": "other/v2",
                "name": "wrong-schema",
                "description": "description",
                "instructions": "instructions",
            })
            .to_string(),
        ),
        (
            "missing-instructions",
            json!({
                "schema": SKILL_MANIFEST_SCHEMA,
                "name": "missing-instructions",
                "description": "description",
            })
            .to_string(),
        ),
        (
            "executable-fields",
            json!({
                "schema": SKILL_MANIFEST_SCHEMA,
                "name": "executable-fields",
                "description": "description",
                "instructions": "instructions",
                "tools": ["shell"],
                "grants": ["network"],
            })
            .to_string(),
        ),
        (
            "bad-name",
            json!({
                "schema": SKILL_MANIFEST_SCHEMA,
                "name": "bad.name",
                "description": "description",
                "instructions": "instructions",
            })
            .to_string(),
        ),
        (
            "name-mismatch",
            json!({
                "schema": SKILL_MANIFEST_SCHEMA,
                "name": "different",
                "description": "description",
                "instructions": "instructions",
            })
            .to_string(),
        ),
        (
            "empty-instructions",
            json!({
                "schema": SKILL_MANIFEST_SCHEMA,
                "name": "empty-instructions",
                "description": "description",
                "instructions": "",
            })
            .to_string(),
        ),
        (
            "oversized-instructions",
            json!({
                "schema": SKILL_MANIFEST_SCHEMA,
                "name": "oversized-instructions",
                "description": "description",
                "instructions": "x".repeat(8 * 1024 + 1),
            })
            .to_string(),
        ),
        (
            "control-character",
            json!({
                "schema": SKILL_MANIFEST_SCHEMA,
                "name": "control-character",
                "description": "description\0",
                "instructions": "instructions",
            })
            .to_string(),
        ),
    ];

    for (stem, contents) in cases {
        let relative = format!("custom/{stem}.json");
        fixture.write(&relative, &contents);
        assert_eq!(
            load_skills(&fixture.root, &[PathBuf::from(&relative)]),
            Err(SkillLoadError::InvalidManifest),
            "{stem}"
        );
    }
}

#[test]
fn rejects_invalid_manifest_filename() {
    let fixture = Fixture::new("bad-filename");
    fixture.manifest(".krypper/skills/bad.name.json", "bad.name");
    assert_eq!(
        load_skills(&fixture.root, &[]),
        Err(SkillLoadError::InvalidManifest)
    );
}

#[test]
fn rejects_conflicting_skill_names() {
    let fixture = Fixture::new("conflict");
    fixture.manifest(".krypper/skills/alpha.json", "alpha");
    fixture.manifest("custom/alpha.json", "alpha");
    assert_eq!(
        load_skills(&fixture.root, &[PathBuf::from("custom/alpha.json")]),
        Err(SkillLoadError::DuplicateName)
    );
}

#[test]
fn enforces_manifest_count_limit() {
    let fixture = Fixture::new("limit");
    for index in 0..MAX_SKILL_MANIFESTS {
        let name = format!("skill{index:03}");
        fixture.manifest(&format!(".krypper/skills/{name}.json"), &name);
    }
    assert_eq!(
        load_skills(&fixture.root, &[]).unwrap().len(),
        MAX_SKILL_MANIFESTS
    );

    fixture.manifest("custom/extra.json", "extra");
    assert_eq!(
        load_skills(&fixture.root, &[PathBuf::from("custom/extra.json")]),
        Err(SkillLoadError::TooManyManifests)
    );
}

#[test]
fn loading_changes_no_registered_tool_or_workspace_file() {
    let fixture = Fixture::new("regression");
    let before = registry::workspace_operations()
        .iter()
        .map(|operation| operation.name())
        .collect::<Vec<_>>();
    fixture.manifest(".krypper/skills/alpha.json", "alpha");
    let contents = fs::read_to_string(fixture.path.join(".krypper/skills/alpha.json")).unwrap();

    let skills = load_skills(&fixture.root, &[]).unwrap();
    let after = registry::workspace_operations()
        .iter()
        .map(|operation| operation.name())
        .collect::<Vec<_>>();

    assert_eq!(before, after);
    assert_eq!(skills.len(), 1);
    assert_eq!(
        fs::read_to_string(fixture.path.join(".krypper/skills/alpha.json")).unwrap(),
        contents
    );
    assert!(!Path::new(skills[0].instructions.as_str()).is_absolute());
}
