//! Unit tests for the model adapter boundary.
use crate::agent_loop::GoalTurnResult;
use crate::brain::*;
use crate::task_lifecycle::{TaskSession, TaskState, TurnOutcome, TurnState};
use crate::test_support::{with_current_dir, TempDir};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::thread::{self, JoinHandle};
use uuid::Uuid;
fn in_workspace<T>(path: &Path, operation: impl FnOnce() -> T) -> T {
    with_current_dir(path, operation)
}

fn assert_failed_turn(
    result: &GoalTurnResult,
    session: &TaskSession,
    task_id: Uuid,
    turn_id: Uuid,
) {
    assert!(matches!(
        result.outcome(),
        Some(TurnOutcome::Failure { .. })
    ));
    let task = session.get_task(task_id).expect("task exists");
    assert_eq!(task.state(), TaskState::Running);
    let turn = task.get_turn(turn_id).expect("turn exists");
    assert_eq!(turn.state(), TurnState::Failed);
    assert!(matches!(turn.outcome(), Some(TurnOutcome::Failure { .. })));
}

fn running_turn() -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new();
    let task = session.create_task("Complete the requested workspace task");
    task.transition(TaskState::Running).expect("start task");
    let task_id = task.id();
    let turn_id = task.create_turn().expect("create turn");
    task.transition_turn(turn_id, TurnState::Running)
        .expect("start turn");
    (session, task_id, turn_id)
}

fn running_turn_in_workspace(workspace: &Path, goal: &str) -> (TaskSession, Uuid, Uuid) {
    let mut session = TaskSession::new_in_workspace(workspace).expect("bind task workspace");
    let task = session.create_task(goal);
    task.transition(TaskState::Running).expect("start task");
    let task_id = task.id();
    let turn_id = task.create_turn().expect("create turn");
    task.transition_turn(turn_id, TurnState::Running)
        .expect("start turn");
    (session, task_id, turn_id)
}

fn reflection_response(content: &str) -> String {
    json!({"message":{"role":"assistant","content":content}}).to_string()
}

struct MockResponse {
    status: u16,
    body: String,
}

fn ok(body: String) -> MockResponse {
    MockResponse { status: 200, body }
}

fn tool_response(name: &str, arguments: Value) -> String {
    json!({
        "message": {
            "role": "assistant",
            "tool_calls": [{
                "function": { "name": name, "arguments": arguments }
            }]
        }
    })
    .to_string()
}

fn mock_server(responses: Vec<MockResponse>) -> (SocketAddr, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock Ollama server");
    let address = listener.local_addr().expect("read mock server address");
    let handle = thread::spawn(move || {
        responses
                .into_iter()
                .map(|response| {
                    let (mut stream, _) = listener.accept().expect("accept model request");
                    let request = read_request(&mut stream);
                    let reason = if response.status == 200 { "OK" } else { "Failure" };
                    write!(
                        stream,
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.status,
                        reason,
                        response.body.len(),
                        response.body
                    )
                    .expect("write mock response");
                    request
                })
                .collect()
    });
    (address, handle)
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream.read(&mut chunk).expect("read mock request");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .expect("request content length");
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    request
}

fn request_json(request: &[u8]) -> Value {
    let body_start = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("request header terminator")
        + 4;
    serde_json::from_slice(&request[body_start..]).expect("decode model request")
}

fn request_header(request: &[u8], wanted: &str) -> Option<String> {
    let header_end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
    String::from_utf8_lossy(&request[..header_end])
        .lines()
        .skip(1)
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(wanted)
                .then(|| value.trim().to_owned())
        })
}

fn request_body(request: &[u8]) -> Value {
    let body_start = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("request header terminator")
        + 4;
    serde_json::from_slice(&request[body_start..]).expect("decode provider request")
}

fn all_advertised_tools() -> AdvertisedToolNames {
    AdvertisedToolNames::all_builtins()
}

mod completion;
#[cfg(unix)]
mod correction;
mod endpoint;
mod guidance;
mod impact;
mod ollama_plan;
mod parser;
mod planner;
mod providers;
mod reflection;
mod registry;
mod requests;
mod turns;
