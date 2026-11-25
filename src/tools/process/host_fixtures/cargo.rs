//! Offline Cargo behavior through the registered Harness path, gated on a real host.

use super::{gate, grant_policy, policy, workspace};
use std::fs;

#[test]
#[ignore = "opt-in real-sandbox evidence: set KRYPPER_REAL_SANDBOX=1 on a provisioned Linux host"]
fn real_offline_cargo_behavior_is_preserved_through_the_harness() {
    let Some(host) = gate("real_offline_cargo_behavior_is_preserved_through_the_harness") else {
        return;
    };
    let path = workspace();
    fs::write(
        path.join("src/lib.rs"),
        "#[test] fn selected_test() { assert_eq!(2 + 2, 4); }\n\
         #[test] fn unrelated_test() { panic!(\"unrelated test ran\"); }\n",
    )
    .unwrap();
    let (mut session, task, turn) = grant_policy(&path, policy(&host));
    use crate::agent_loop::{
        run_goal_turn_with_plan, GoalTurnStopReason, WorkspaceAction, WorkspaceObservation,
    };
    let plan = crate::WorkspacePlan::try_from_actions(
        vec![WorkspaceAction::OfflineCargoTest {
            test_filter: Some("selected_test".into()),
        }],
        1,
    );
    let mut control = |_: &str, observations: &[WorkspaceObservation]| {
        assert!(matches!(
            observations,
            [WorkspaceObservation::ProcessFinished { result }]
                if result.succeeded() && result.executed_tests == Some(1)
        ));
        WorkspaceAction::Complete {
            result: "offline Cargo verified".into(),
        }
    };
    let execution = run_goal_turn_with_plan(
        &mut session,
        task,
        turn,
        "real sandbox fixture",
        2,
        plan,
        &mut control,
    )
    .unwrap();
    assert_eq!(
        execution.stop_reason(),
        GoalTurnStopReason::Completed,
        "{execution:?}"
    );
    let [WorkspaceObservation::ProcessFinished { result }] = execution.observations() else {
        panic!("one process observation required")
    };
    assert!(result.succeeded(), "{result:?}");
    assert_eq!(result.test_filter.as_deref(), Some("selected_test"));
    assert_eq!(result.executed_tests, Some(1));
}
