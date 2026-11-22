use super::*;

#[test]
fn final_actions_require_full_ordered_completion_and_reject_replay() {
    let mut actions = ResponsesFinalActions::new(2, 128);
    let frames = [
        (
            "response.output_item.added",
            serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
            }),
        ),
        (
            "response.function_call_arguments.delta",
            serde_json::json!({
                "output_index": 0, "delta": r#"{"path":"src"}"#
            }),
        ),
        (
            "response.function_call_arguments.done",
            serde_json::json!({
                "output_index": 0
            }),
        ),
        (
            "response.output_item.done",
            serde_json::json!({
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
            }),
        ),
        (
            "response.completed",
            serde_json::json!({
                "response": {"status": "completed"}
            }),
        ),
    ];
    for (index, (event, data)) in frames.iter().enumerate() {
        let frame = SseFrame {
            event: Some((*event).into()),
            data: data.to_string(),
        };
        let result = actions.consume(&frame).expect("valid stream event");
        if index == 4 {
            assert_eq!(
                result,
                Some(vec![(
                    "call_a".to_owned(),
                    "run".to_owned(),
                    serde_json::json!({"path": "src"})
                )])
            );
            assert_eq!(actions.consume(&frame), Err(ArgumentsError::InvalidEvent));
        } else {
            assert_eq!(result, None);
        }
    }
}

#[test]
fn final_actions_reject_unregistered_argument_fragments() {
    let mut actions = ResponsesFinalActions::new(1, 128);
    let delta = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
    };
    assert_eq!(
        actions.consume(&delta),
        Err(ArgumentsError::UnexpectedIndex)
    );
    assert_eq!(actions.consume(&delta), Err(ArgumentsError::InvalidEvent));
}

#[test]
fn final_actions_reject_completed_response_without_tool_call() {
    let mut actions = ResponsesFinalActions::new(1, 128);
    let completed = SseFrame {
        event: Some("response.completed".into()),
        data: serde_json::json!({"response": {"status": "completed"}}).to_string(),
    };
    assert_eq!(
        actions.consume(&completed),
        Err(ArgumentsError::InvalidEvent)
    );
}

#[test]
fn final_actions_fail_closed_after_provider_failure() {
    let mut actions = ResponsesFinalActions::new(1, 64);
    let failed = SseFrame {
        event: Some("response.failed".into()),
        data: "{}".into(),
    };
    assert_eq!(actions.consume(&failed), Err(ArgumentsError::InvalidEvent));
    let completed = SseFrame {
        event: Some("response.completed".into()),
        data: serde_json::json!({"response": {"status": "completed"}}).to_string(),
    };
    assert_eq!(
        actions.consume(&completed),
        Err(ArgumentsError::InvalidEvent)
    );
}

#[test]
fn finalization_rejects_missing_output_item_completion() {
    let mut batch = ResponsesCallBatch::new(1, 64);
    let added = SseFrame {
        event: Some("response.output_item.added".into()),
        data: serde_json::json!({
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
        })
        .to_string(),
    };
    assert_eq!(batch.register_call(&added), Ok(true));
    let delta = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
    };
    assert_eq!(batch.push(&delta), Ok(true));
    let done = SseFrame {
        event: Some("response.function_call_arguments.done".into()),
        data: serde_json::json!({"output_index": 0}).to_string(),
    };
    assert_eq!(batch.complete(&done), Ok(true));
    let completed = SseFrame {
        event: Some("response.completed".into()),
        data: serde_json::json!({"response": {"status": "completed"}}).to_string(),
    };
    assert_eq!(
        batch.finalized_named_calls(&completed),
        Err(ArgumentsError::InvalidEvent)
    );
}

#[test]
fn output_item_completion_rejects_unregistered_identity() {
    let mut batch = ResponsesCallBatch::new(1, 64);
    let done = SseFrame {
        event: Some("response.output_item.done".into()),
        data: serde_json::json!({
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
        })
        .to_string(),
    };
    assert_eq!(
        batch.complete_output_item(&done),
        Err(ArgumentsError::InvalidEvent)
    );
}

#[test]
fn finalization_requires_successful_response_completion() {
    let batch = ResponsesCallBatch::new(1, 64);
    let incomplete = SseFrame {
        event: Some("response.incomplete".into()),
        data: serde_json::json!({"response": {"status": "incomplete"}}).to_string(),
    };
    assert_eq!(
        batch.finalized_named_calls(&incomplete),
        Err(ArgumentsError::InvalidEvent)
    );
    let failed = SseFrame {
        event: Some("response.completed".into()),
        data: serde_json::json!({"response": {"status": "failed"}}).to_string(),
    };
    assert_eq!(
        batch.finalized_named_calls(&failed),
        Err(ArgumentsError::InvalidEvent)
    );
    let completed = SseFrame {
        event: Some("response.completed".into()),
        data: serde_json::json!({"response": {"status": "completed"}}).to_string(),
    };
    assert_eq!(batch.finalized_named_calls(&completed), Ok(vec![]));
}

#[test]
fn named_calls_require_name_id_and_completion() {
    let mut batch = ResponsesCallBatch::new(1, 64);
    let added = SseFrame {
        event: Some("response.output_item.added".into()),
        data: serde_json::json!({
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
        })
        .to_string(),
    };
    assert_eq!(batch.register_call(&added), Ok(true));
    assert!(batch.correlated_named_calls().is_err());
    let delta = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
    };
    assert_eq!(batch.push(&delta), Ok(true));
    let done = SseFrame {
        event: Some("response.function_call_arguments.done".into()),
        data: serde_json::json!({"output_index": 0}).to_string(),
    };
    assert_eq!(batch.complete(&done), Ok(true));
    assert_eq!(
        batch.correlated_named_calls(),
        Ok(vec![(
            "call_a".to_owned(),
            "run".to_owned(),
            serde_json::json!({})
        )])
    );
}

#[test]
fn correlated_calls_require_registered_id_and_done_event() {
    let mut batch = ResponsesCallBatch::new(2, 64);
    let delta = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
    };
    assert_eq!(batch.push(&delta), Ok(true));
    assert!(batch.correlated_completed_calls().is_err());
    let added = SseFrame {
        event: Some("response.output_item.added".into()),
        data: serde_json::json!({
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
        })
        .to_string(),
    };
    assert_eq!(batch.register_call(&added), Ok(true));
    assert!(batch.correlated_completed_calls().is_err());
    let done = SseFrame {
        event: Some("response.function_call_arguments.done".into()),
        data: serde_json::json!({"output_index": 0}).to_string(),
    };
    assert_eq!(batch.complete(&done), Ok(true));
    assert_eq!(
        batch.correlated_completed_calls(),
        Ok(vec![("call_a".to_owned(), serde_json::json!({}))])
    );
}

#[test]
fn responses_batch_correlates_call_ids_and_rejects_duplicates() {
    let mut batch = ResponsesCallBatch::new(2, 64);
    let first = SseFrame {
        event: Some("response.output_item.added".into()),
        data: serde_json::json!({
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
        })
        .to_string(),
    };
    assert_eq!(batch.register_call(&first), Ok(true));
    let duplicate = SseFrame {
        event: Some("response.output_item.added".into()),
        data: serde_json::json!({
            "output_index": 1,
            "item": {"type": "function_call", "call_id": "call_a", "name": "run"}
        })
        .to_string(),
    };
    assert_eq!(
        batch.register_call(&duplicate),
        Err(ArgumentsError::InvalidEvent)
    );
    assert_eq!(
        batch.register_call(&first),
        Err(ArgumentsError::InvalidEvent)
    );
}

#[test]
fn responses_batch_requires_done_before_exposing_completed_arguments() {
    let mut batch = ResponsesCallBatch::new(2, 64);
    let delta = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 0, "delta": "{}"}).to_string(),
    };
    assert_eq!(batch.push(&delta), Ok(true));
    assert_eq!(batch.completed_arguments(), Ok(vec![]));
    let done = SseFrame {
        event: Some("response.function_call_arguments.done".into()),
        data: serde_json::json!({"output_index": 0}).to_string(),
    };
    assert_eq!(batch.complete(&done), Ok(true));
    assert_eq!(
        batch.completed_arguments(),
        Ok(vec![(0, serde_json::json!({}))])
    );
    assert_eq!(batch.complete(&done), Err(ArgumentsError::InvalidEvent));
    assert_eq!(batch.push(&delta), Err(ArgumentsError::InvalidEvent));
}

#[test]
fn responses_batch_keeps_interleaved_calls_separate() {
    let mut batch = ResponsesCallBatch::new(2, 64);
    for (index, delta) in [(1, "{"), (0, "{}"), (1, "}")] {
        let frame = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": index, "delta": delta}).to_string(),
        };
        assert_eq!(batch.push(&frame), Ok(true));
    }
    assert_eq!(
        batch.parsed(),
        Ok(vec![(0, serde_json::json!({})), (1, serde_json::json!({}))])
    );
    let extra = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 2, "delta": "{}"}).to_string(),
    };
    assert_eq!(batch.push(&extra), Err(ArgumentsError::TooLarge));
}

#[test]
fn responses_arguments_collect_fragments_without_executing() {
    let mut collector = ResponsesArguments::new(64);
    for (index, delta) in [(0, r#"{"path":"#), (0, r#""src"}"#)] {
        let frame = SseFrame {
            event: Some("response.function_call_arguments.delta".into()),
            data: serde_json::json!({"output_index": index, "delta": delta}).to_string(),
        };
        assert_eq!(collector.push(&frame), Ok(true));
    }
    assert_eq!(
        collector.parse_completed(),
        Ok(serde_json::json!({"path": "src"}))
    );
    let wrong = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 1, "delta": "{}"}).to_string(),
    };
    assert_eq!(collector.push(&wrong), Err(ArgumentsError::UnexpectedIndex));
}

#[test]
fn responses_arguments_reject_oversize_and_incomplete_json() {
    let mut collector = ResponsesArguments::new(4);
    let fragment = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 0, "delta": "{"}).to_string(),
    };
    assert_eq!(collector.push(&fragment), Ok(true));
    assert_eq!(
        collector.parse_completed(),
        Err(ArgumentsError::InvalidJson)
    );
    let oversize = SseFrame {
        event: Some("response.function_call_arguments.delta".into()),
        data: serde_json::json!({"output_index": 0, "delta": "12345"}).to_string(),
    };
    assert_eq!(collector.push(&oversize), Err(ArgumentsError::TooLarge));
}
