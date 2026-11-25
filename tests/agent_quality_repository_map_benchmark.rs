mod agent_quality {
    #[path = "support.rs"]
    pub mod support;
}

use agent_quality::support::TempRoot;
use krypper::harness::context::{
    collect_repository_context, RepositoryContext, MAX_REPOSITORY_CONTEXT_BYTES,
};
use krypper::harness::repository_map::{collect_repository_module_map, RepositoryModuleMap};
use krypper::task_lifecycle::{TaskSession, TaskState, TurnState};
use ring::digest;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

const BASELINE_SHA: &str = "5c4ce1cd8f05d2dec93c545f67608c28d8ddb7e8";
const FIXTURE_SHA256: &str = "68c9d7aa4dc3f722777fe55e1a6ff37013a12428c42b53fb142f4c5a1a7e1842";
const SYNTHETIC_PRODUCT_FIXTURE_SHA256: &str =
    "337700a138e8d43499f5cc4e0897797f96e8e63780b44792bf2b7c2bcadb32d6";
const REPEAT_RUNS: usize = 3;

struct TaskCase {
    name: &'static str,
    goal: &'static str,
    expected_paths: &'static [&'static str],
}

const TASKS: &[TaskCase] = &[
    TaskCase {
        name: "nested parser bug location",
        goal: "Fix parser tokenization behavior",
        expected_paths: &["src/parser/tokens.rs"],
    },
    TaskCase {
        name: "API response bug location",
        goal: "Update API response handling",
        expected_paths: &["src/api/response.rs"],
    },
    TaskCase {
        name: "cache bug location",
        goal: "Guard stale cache reads in store",
        expected_paths: &["src/store/cache.rs"],
    },
    TaskCase {
        name: "parser and API cross-file location",
        goal: "Update parser tokens and API response",
        expected_paths: &["src/parser/tokens.rs", "src/api/response.rs"],
    },
];

const PRODUCT_TASKS: &[TaskCase] = &[
    TaskCase {
        name: "workspace search implementation",
        goal: "Locate workspace search implementation",
        expected_paths: &["src/tools/workspace_safety/search.rs"],
    },
    TaskCase {
        name: "lifecycle session implementation",
        goal: "Locate lifecycle session implementation",
        expected_paths: &["src/core/lifecycle/session.rs"],
    },
    TaskCase {
        name: "harness context implementation",
        goal: "Locate harness context implementation",
        expected_paths: &["src/harness/context.rs"],
    },
];

#[test]
fn matched_payload_budget_compares_bounded_search_with_the_module_map() {
    let root = TempRoot::new("repository-map-benchmark").expect("create benchmark root");
    let workspace = root.path().join("workspace");
    setup_workspace(&workspace);
    let fixture_digest = fixture_digest(&workspace);
    let mut session = TaskSession::new_in_workspace(&workspace).expect("bind fixture workspace");

    let mut baseline_hits = 0;
    let mut map_hits = 0;
    let mut expected_hits = 0;
    let mut baseline_bytes = 0;
    let mut map_bytes = 0;
    let mut map_source_bytes = 0;
    let mut task_hits = BTreeMap::<&str, (usize, usize)>::new();
    let mut baseline_micros = Vec::new();
    let mut map_micros = Vec::new();
    for task in TASKS {
        for run in 0..REPEAT_RUNS {
            let (task_id, turn_id) = running_turn(&mut session, task.goal);
            let (search, map, search_elapsed, map_elapsed) =
                measure_pair(&session, task_id, turn_id, task.goal, run % 2 == 1);
            baseline_micros.push(search_elapsed);
            map_micros.push(map_elapsed);

            assert!(search.bytes <= MAX_REPOSITORY_CONTEXT_BYTES);
            assert!(map.bytes <= MAX_REPOSITORY_CONTEXT_BYTES);
            assert!(map.files_examined <= 64);
            assert!(map.source_bytes_read <= 3 * 1024 * 1024);
            baseline_bytes += search.bytes;
            map_bytes += map.bytes;
            map_source_bytes += map.source_bytes_read;

            for expected in task.expected_paths {
                expected_hits += 1;
                let baseline_found = search
                    .snippets
                    .iter()
                    .any(|snippet| snippet.path == *expected);
                baseline_hits += usize::from(baseline_found);
                let task_hit = task_hits.entry(task.name).or_default();
                task_hit.0 += usize::from(baseline_found);
                task_hit.1 += 1;
                let map_reaches_expected = map.modules.iter().any(|module| {
                    module
                        .conventional_path_candidates
                        .as_ref()
                        .is_some_and(|paths| paths.iter().any(|path| path == expected))
                });
                map_hits += usize::from(map_reaches_expected);
                assert!(
                    map_reaches_expected,
                    "{} did not map to {expected}: {:?}",
                    task.name, map.modules
                );
            }
        }
    }

    assert!(baseline_hits <= expected_hits);
    assert_eq!(expected_hits, 15);
    assert_eq!(map_hits, expected_hits);
    assert_eq!(fixture_digest, FIXTURE_SHA256);
    eprintln!(
        "baseline_sha={BASELINE_SHA} fixture_sha256={fixture_digest} tasks={} repeats={} payload_budget_bytes={} model=none provider_calls=0 baseline_target_hits={baseline_hits}/{expected_hits} module_map_target_hits={map_hits}/{expected_hits} baseline_avg_payload_bytes={} module_map_avg_payload_bytes={} module_map_avg_source_bytes={} baseline_p50_us={} module_map_p50_us={}",
        TASKS.len(),
        REPEAT_RUNS,
        MAX_REPOSITORY_CONTEXT_BYTES,
        baseline_bytes / (TASKS.len() * REPEAT_RUNS),
        map_bytes / (TASKS.len() * REPEAT_RUNS),
        map_source_bytes / (TASKS.len() * REPEAT_RUNS),
        median_micros(&baseline_micros),
        median_micros(&map_micros),
    );
    eprintln!("task_target_hits={task_hits:?}");
}

#[test]
fn matched_payload_budget_covers_a_synthetic_product_layout() {
    let root = TempRoot::new("repository-map-product-layout").expect("create layout fixture root");
    let workspace = root.path().join("workspace");
    write_synthetic_product_layout(&workspace);
    let fixture_digest = fixture_digest(&workspace);
    let mut session = TaskSession::new_in_workspace(&workspace).expect("bind source fixture");

    let mut baseline_hits = 0;
    let mut map_hits = 0;
    let mut expected_hits = 0;
    let mut baseline_bytes = 0;
    let mut map_bytes = 0;
    let mut map_source_bytes = 0;
    let mut baseline_micros = Vec::new();
    let mut map_micros = Vec::new();
    for task in PRODUCT_TASKS {
        for run in 0..REPEAT_RUNS {
            let (task_id, turn_id) = running_turn(&mut session, task.goal);
            let (search, map, search_elapsed, map_elapsed) =
                measure_pair(&session, task_id, turn_id, task.goal, run % 2 == 1);
            baseline_micros.push(search_elapsed);
            map_micros.push(map_elapsed);

            assert!(search.bytes <= MAX_REPOSITORY_CONTEXT_BYTES);
            assert!(map.bytes <= MAX_REPOSITORY_CONTEXT_BYTES);
            assert!(map.source_bytes_read <= 3 * 1024 * 1024);
            baseline_bytes += search.bytes;
            map_bytes += map.bytes;
            map_source_bytes += map.source_bytes_read;
            for expected in task.expected_paths {
                expected_hits += 1;
                baseline_hits += usize::from(
                    search
                        .snippets
                        .iter()
                        .any(|snippet| snippet.path == *expected),
                );
                let map_reaches_expected = map.modules.iter().any(|module| {
                    module
                        .conventional_path_candidates
                        .as_ref()
                        .is_some_and(|paths| paths.iter().any(|path| path == expected))
                });
                map_hits += usize::from(map_reaches_expected);
                assert!(
                    map_reaches_expected,
                    "{} did not map to {expected}",
                    task.name
                );
            }
        }
    }
    assert_eq!(map_hits, expected_hits);
    assert_eq!(expected_hits, 9);
    assert_eq!(fixture_digest, SYNTHETIC_PRODUCT_FIXTURE_SHA256);
    eprintln!(
        "baseline_sha={BASELINE_SHA} synthetic_product_fixture_sha256={fixture_digest} tasks={} repeats={} payload_budget_bytes={} model=none provider_calls=0 baseline_target_hits={baseline_hits}/{expected_hits} module_map_target_hits={map_hits}/{expected_hits} baseline_avg_payload_bytes={} module_map_avg_payload_bytes={} module_map_avg_source_bytes={} baseline_p50_us={} module_map_p50_us={}",
        PRODUCT_TASKS.len(),
        REPEAT_RUNS,
        MAX_REPOSITORY_CONTEXT_BYTES,
        baseline_bytes / (PRODUCT_TASKS.len() * REPEAT_RUNS),
        map_bytes / (PRODUCT_TASKS.len() * REPEAT_RUNS),
        map_source_bytes / (PRODUCT_TASKS.len() * REPEAT_RUNS),
        median_micros(&baseline_micros),
        median_micros(&map_micros),
    );
}

fn measure_pair(
    session: &TaskSession,
    task_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    goal: &str,
    map_first: bool,
) -> (RepositoryContext, RepositoryModuleMap, u128, u128) {
    let (search, search_elapsed, map, map_elapsed) = if map_first {
        let started = Instant::now();
        let map = collect_repository_module_map(session, task_id, turn_id, goal)
            .expect("collect bounded module map");
        let map_elapsed = started.elapsed().as_micros();
        let started = Instant::now();
        let search = collect_repository_context(session, task_id, turn_id, goal)
            .expect("collect bounded-search baseline");
        (search, started.elapsed().as_micros(), map, map_elapsed)
    } else {
        let started = Instant::now();
        let search = collect_repository_context(session, task_id, turn_id, goal)
            .expect("collect bounded-search baseline");
        let search_elapsed = started.elapsed().as_micros();
        let started = Instant::now();
        let map = collect_repository_module_map(session, task_id, turn_id, goal)
            .expect("collect bounded module map");
        (search, search_elapsed, map, started.elapsed().as_micros())
    };
    (search, map, search_elapsed, map_elapsed)
}

fn median_micros(samples: &[u128]) -> u128 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn setup_workspace(workspace: &Path) {
    fs::create_dir_all(workspace.join("src/parser")).expect("create parser module tree");
    fs::create_dir_all(workspace.join("src/api")).expect("create API module tree");
    fs::create_dir_all(workspace.join("src/store")).expect("create store module tree");
    fs::create_dir_all(workspace.join("ignored")).expect("create ignored directory");
    fs::create_dir_all(workspace.join(".git")).expect("create Git metadata");
    fs::write(workspace.join(".gitignore"), "ignored/\n").expect("write ignore rules");
    fs::write(
        workspace.join("README.md"),
        "Fix parser startup notes.\nUpdate API examples.\nGuard cache startup notes.\n",
    )
    .expect("write distractor documentation");
    fs::write(
        workspace.join("src/lib.rs"),
        "pub mod api;\npub mod parser;\npub mod store;\n",
    )
    .expect("write crate root");
    fs::write(workspace.join("src/parser.rs"), "pub mod tokens;\n")
        .expect("write parser declaration");
    fs::write(
        workspace.join("src/parser/tokens.rs"),
        "pub fn parse_tokens(input: &str) -> usize { input.len() }\n",
    )
    .expect("write parser target");
    fs::write(workspace.join("src/api.rs"), "pub mod response;\n").expect("write API declaration");
    fs::write(
        workspace.join("src/api/response.rs"),
        "pub fn render_response(status: u16) -> String { status.to_string() }\n",
    )
    .expect("write API target");
    fs::write(workspace.join("src/store.rs"), "pub mod cache;\n").expect("write store declaration");
    fs::write(
        workspace.join("src/store/cache.rs"),
        "pub fn read_cache(key: &str) -> Option<String> { Some(key.to_owned()) }\n",
    )
    .expect("write cache target");
    fs::write(workspace.join("ignored/secret.rs"), "pub mod secret;\n")
        .expect("write ignored source");
    fs::write(workspace.join(".git/metadata.rs"), "pub mod metadata;\n")
        .expect("write Git metadata source");
}

fn write_synthetic_product_layout(workspace: &Path) {
    let files = [
        (
            "src/lib.rs",
            "pub mod core;\npub mod harness;\npub mod tools;\n",
        ),
        ("src/core/mod.rs", "pub mod lifecycle;\n"),
        (
            "src/core/lifecycle.rs",
            "pub mod checkpoint;\npub mod errors;\npub mod session;\n",
        ),
        (
            "src/core/lifecycle/checkpoint.rs",
            "pub struct Checkpoint;\n",
        ),
        (
            "src/core/lifecycle/errors.rs",
            "pub enum LifecycleError { InvalidState }\n",
        ),
        (
            "src/core/lifecycle/session.rs",
            "pub struct TaskSession;\nimpl TaskSession { pub fn active_turn(&self) -> bool { true } }\n",
        ),
        (
            "src/harness/mod.rs",
            "pub mod context;\npub mod recovery;\n",
        ),
        (
            "src/harness/context.rs",
            "pub fn collect_repository_context() -> Vec<String> { Vec::new() }\n",
        ),
        ("src/harness/recovery.rs", "pub struct RecoveryPoint;\n"),
        ("src/tools.rs", "pub mod workspace_safety;\n"),
        (
            "src/tools/workspace_safety.rs",
            "pub mod paths;\npub mod search;\n",
        ),
        (
            "src/tools/workspace_safety/paths.rs",
            "pub fn resolve_workspace_path() -> bool { true }\n",
        ),
        (
            "src/tools/workspace_safety/search.rs",
            "pub fn search_workspace() -> Vec<String> { Vec::new() }\n",
        ),
    ];
    for (relative, contents) in files {
        let path = workspace.join(relative);
        fs::create_dir_all(path.parent().expect("synthetic fixture parent"))
            .expect("create synthetic fixture directory");
        fs::write(path, contents).expect("write synthetic fixture file");
    }
}

fn fixture_digest(workspace: &Path) -> String {
    let mut files = Vec::new();
    collect_fixture_files(workspace, workspace, &mut files);
    files.sort();
    let serialized = files
        .into_iter()
        .map(|(path, contents)| format!("{path}\0{contents}\0"))
        .collect::<String>();
    digest::digest(&digest::SHA256, serialized.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn collect_fixture_files(root: &Path, directory: &Path, files: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(directory).expect("read fixture directory") {
        let entry = entry.expect("read fixture entry");
        let path = entry.path();
        if entry.file_type().expect("read fixture entry type").is_dir() {
            collect_fixture_files(root, &path, files);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("fixture file remains in root")
                .to_string_lossy()
                .into_owned();
            let contents = fs::read_to_string(&path).expect("read fixture file");
            files.push((relative, contents));
        }
    }
}

fn running_turn(session: &mut TaskSession, goal: &str) -> (uuid::Uuid, uuid::Uuid) {
    let task_id = session.create_task(goal.to_owned()).id();
    let task = session.get_task_mut(task_id).expect("created fixture task");
    task.transition(TaskState::Running)
        .expect("start fixture task");
    let turn_id = task.create_turn().expect("create fixture turn");
    task.transition_turn(turn_id, TurnState::Running)
        .expect("start fixture turn");
    (task_id, turn_id)
}
