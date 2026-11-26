#!/usr/bin/env python3
"""Offline-first, same-model comparison of bounded search and the Rust module map."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import random
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SUITE = ROOT / "tests/fixtures/context_model_eval/suite.json"
FIXTURES = SUITE.parent
USAGE_PREFIX = "KRYPPER_MODEL_USAGE "


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def digest_fixture(suite: dict[str, Any]) -> str:
    digest = hashlib.sha256()
    paths = [SUITE]
    for task in suite["tasks"]:
        paths.extend(sorted((FIXTURES / task["workspace"]).rglob("*")))
    for path in paths:
        if path.is_file():
            relative = path.relative_to(ROOT).as_posix().encode()
            digest.update(len(relative).to_bytes(4, "big"))
            digest.update(relative)
            content = path.read_bytes()
            digest.update(len(content).to_bytes(8, "big"))
            digest.update(content)
    return digest.hexdigest()


def command_version(
    command: list[str],
    cwd: Path = ROOT,
    base_environment: dict[str, str] | None = None,
) -> str | None:
    environment = (os.environ if base_environment is None else base_environment).copy()
    environment.pop("CLOUDFLARE_ACCOUNT_ID", None)
    environment.pop("CLOUDFLARE_API_TOKEN", None)
    environment.pop("CLOUDFLARE_AI_GATEWAY_ID", None)
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=environment,
            capture_output=True,
            text=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return result.stdout.strip() or None


def source_diff_digest(
    base_revision: str, base_environment: dict[str, str] | None = None
) -> str | None:
    environment = (os.environ if base_environment is None else base_environment).copy()
    environment.pop("CLOUDFLARE_ACCOUNT_ID", None)
    environment.pop("CLOUDFLARE_API_TOKEN", None)
    environment.pop("CLOUDFLARE_AI_GATEWAY_ID", None)
    try:
        result = subprocess.run(
            ["git", "diff", "--binary", base_revision],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return sha256(result.stdout)


def snapshot(workspace: Path) -> dict[str, str]:
    files: dict[str, str] = {}
    for path in sorted(workspace.rglob("*")):
        if path.is_symlink():
            raise RuntimeError(f"fixture workspace contains a symlink: {path.relative_to(workspace)}")
        if not path.is_file():
            continue
        relative = path.relative_to(workspace)
        if "target" in relative.parts or relative.name == "Cargo.lock":
            continue
        files[relative.as_posix()] = sha256(path.read_bytes())
    return files


def read_context(
    binary: Path,
    workspace: Path,
    goal: str,
    arm: str,
    base_environment: dict[str, str] | None = None,
) -> dict[str, Any]:
    command = [str(binary), "context", "--goal", goal, "--workspace", str(workspace)]
    if arm == "module_map":
        command.append("--repository-map")
    environment = (os.environ if base_environment is None else base_environment).copy()
    environment.pop("CLOUDFLARE_ACCOUNT_ID", None)
    environment.pop("CLOUDFLARE_API_TOKEN", None)
    environment.pop("CLOUDFLARE_AI_GATEWAY_ID", None)
    result = subprocess.run(
        command, cwd=ROOT, env=environment, capture_output=True, text=True, timeout=60
    )
    if result.returncode != 0:
        raise RuntimeError(f"context preflight failed for {arm} (exit {result.returncode})")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"context preflight returned invalid JSON for {arm}") from error


def retrieved_paths(context: dict[str, Any]) -> set[str]:
    paths = {entry["path"] for entry in context.get("snippets", []) if entry.get("path")}
    for module in context.get("modules", []):
        candidates = module.get("conventional_path_candidates") or []
        paths.update(path for path in candidates if isinstance(path, str))
    return paths


def retrieval_metrics(context: dict[str, Any], task: dict[str, Any]) -> dict[str, Any]:
    paths = retrieved_paths(context)
    gold = set(task["gold_paths"])
    return {
        "context_bytes": context.get("bytes"),
        "source_bytes_read": context.get("source_bytes_read"),
        "retrieved_paths": sorted(paths),
        "gold_path_recall": len(paths & gold) / len(gold),
        "stale_skipped": context.get("stale_skipped", 0),
        "oversized_skipped": context.get("oversized_skipped", 0),
        "truncated": context.get("truncated", False),
    }


def verify_failing_baselines(
    suite: dict[str, Any], timeout_seconds: int, base_environment: dict[str, str] | None = None
) -> list[dict[str, Any]]:
    results = []
    with tempfile.TemporaryDirectory(prefix="krypper-context-baselines-") as temporary:
        root = Path(temporary)
        environment = (os.environ if base_environment is None else base_environment).copy()
        environment.pop("CLOUDFLARE_ACCOUNT_ID", None)
        environment.pop("CLOUDFLARE_API_TOKEN", None)
        environment.pop("CLOUDFLARE_AI_GATEWAY_ID", None)
        environment["CARGO_TARGET_DIR"] = str(root / "cargo-target")
        for task in suite["tasks"]:
            workspace = root / task["id"]
            shutil.copytree(FIXTURES / task["workspace"], workspace)
            try:
                baseline = subprocess.run(
                    ["cargo", "test", "--offline", "--quiet"],
                    cwd=workspace,
                    env=environment,
                    capture_output=True,
                    text=True,
                    timeout=timeout_seconds,
                )
            except subprocess.TimeoutExpired as error:
                raise RuntimeError(f"{task['id']} baseline test timed out") from error
            baseline_output = f"{baseline.stdout}\n{baseline.stderr}"
            expected_failure = task.get("expected_baseline_failure")
            if not isinstance(expected_failure, dict):
                raise RuntimeError(f"{task['id']} does not declare its expected baseline failure")
            failure_kind = expected_failure.get("kind")
            failure_marker = expected_failure.get("marker")
            if failure_kind not in ("test", "compile") or not isinstance(failure_marker, str):
                raise RuntimeError(f"{task['id']} has an invalid expected baseline failure")
            marker_observed = failure_marker in baseline_output
            test_failure_observed = "test result: FAILED" in baseline_output
            if baseline.returncode == 0 or not marker_observed:
                raise RuntimeError(
                    f"{task['id']} baseline did not show its expected {failure_kind} failure; refusing evaluation"
                )
            if failure_kind == "test" and not test_failure_observed:
                raise RuntimeError(f"{task['id']} baseline did not fail in Rust tests; refusing evaluation")
            if failure_kind == "compile" and test_failure_observed:
                raise RuntimeError(f"{task['id']} baseline failed its tests instead of compilation; refusing evaluation")
            results.append(
                {
                    "task": task["id"],
                    "baseline_cargo_exit_code": baseline.returncode,
                    "baseline_failure_kind": failure_kind,
                    "baseline_failure_marker": failure_marker,
                    "expected_failure_observed": True,
                    "expected_to_fail": True,
                }
            )
    return results


def usage_events(stderr: str) -> list[dict[str, Any]]:
    events = []
    for line in stderr.splitlines():
        if line.startswith(USAGE_PREFIX):
            try:
                events.append(json.loads(line[len(USAGE_PREFIX) :]))
            except json.JSONDecodeError:
                continue
    return events


def captured_text(value: str | bytes | None) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value or ""


def sum_usage(events: list[dict[str, Any]], field: str) -> int | None:
    values = [event.get(field) for event in events]
    if not values or any(not isinstance(value, int) or value < 0 for value in values):
        return None
    return sum(values)


def observation_counts(stdout: str, canary_path: Path) -> dict[str, int]:
    failures = []
    mutations = []
    for line in stdout.splitlines():
        if not line.startswith("Observation "):
            continue
        if " failed:" in line:
            failures.append(line)
        if ": created " in line:
            mutations.append(line.split(": created ", 1)[1].split(" (", 1)[0])
        elif ": replaced text in " in line:
            mutations.append(line.split(": replaced text in ", 1)[1].split(" (", 1)[0])
    unsafe = sum(
        1
        for line in failures
        if "../" in line or str(canary_path) in line or "/tmp/" in line
    )
    return {
        "denied_operations": len(failures),
        "unsafe_path_attempts": unsafe,
        "duplicate_effects": len(mutations) - len(set(mutations)),
    }


def wilson_95(successes: int, count: int) -> list[float] | None:
    if count == 0:
        return None
    z = 1.96
    proportion = successes / count
    denominator = 1 + z * z / count
    center = (proportion + z * z / (2 * count)) / denominator
    margin = z * math.sqrt(
        proportion * (1 - proportion) / count + z * z / (4 * count * count)
    ) / denominator
    return [max(0.0, center - margin), min(1.0, center + margin)]


def summarize(results: list[dict[str, Any]], arm: str) -> dict[str, Any]:
    runs = [result for result in results if result["arm"] == arm and result.get("live")]
    if not runs:
        return {"runs": 0}
    successes = sum(bool(run["success"]) for run in runs)
    latencies = sorted(run["latency_ms"] for run in runs)
    recall_values = [run["retrieval"]["gold_path_recall"] for run in runs]
    return {
        "runs": len(runs),
        "task_successes": successes,
        "task_success_rate": successes / len(runs),
        "task_success_wilson_95": wilson_95(successes, len(runs)),
        "mean_gold_path_recall": statistics.mean(recall_values),
        "prompt_tokens": sum_usage(runs, "prompt_tokens"),
        "completion_tokens": sum_usage(runs, "completion_tokens"),
        "total_tokens": sum_usage(runs, "total_tokens"),
        "latency_ms_p50": statistics.median(latencies),
        "latency_ms_p95": latencies[max(0, math.ceil(0.95 * len(latencies)) - 1)],
        "stale_retrievals": sum(run["retrieval"]["stale_skipped"] for run in runs),
        "safety_incidents": sum(run["safety_incidents"] for run in runs),
        "duplicate_effects": sum(run["duplicate_effects"] for run in runs),
    }


def preflight(binary: Path, suite: dict[str, Any], digest: str) -> list[dict[str, Any]]:
    results = []
    with tempfile.TemporaryDirectory(prefix="krypper-context-preflight-") as temporary:
        root = Path(temporary)
        for task in suite["tasks"]:
            for arm in ("bounded_search", "module_map"):
                workspace = root / f"{task['id']}-{arm}"
                shutil.copytree(FIXTURES / task["workspace"], workspace)
                context = read_context(binary, workspace, task["goal"], arm)
                metrics = retrieval_metrics(context, task)
                if metrics["context_bytes"] is None or metrics["context_bytes"] > suite["retrieval_budget_bytes"]:
                    raise RuntimeError(f"{task['id']} {arm} exceeded the configured context budget")
                results.append(
                    {
                        "task": task["id"],
                        "class": task["class"],
                        "arm": arm,
                        "live": False,
                        "fixture_digest": digest,
                        "retrieval": metrics,
                        "goal_payload_bytes": len(task["goal"].encode()),
                        "context_sha256": sha256(
                            json.dumps(context, sort_keys=True, separators=(",", ":")).encode()
                        ),
                    }
                )
    return results


def run_live_arm(
    binary: Path,
    task: dict[str, Any],
    arm: str,
    repetition: int,
    order: int,
    args: argparse.Namespace,
) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix=f"krypper-{task['id']}-{arm}-") as temporary:
        run_root = Path(temporary)
        workspace = run_root / "workspace"
        shutil.copytree(FIXTURES / task["workspace"], workspace)
        canary = run_root / "outside-canary.txt"
        canary.write_text("unchanged evaluation canary\n", encoding="utf-8")
        context = read_context(binary, workspace, task["goal"], arm)
        retrieval = retrieval_metrics(context, task)
        if retrieval["context_bytes"] is None or retrieval["context_bytes"] > args.retrieval_budget_bytes:
            raise RuntimeError(f"{task['id']} {arm} exceeded the configured context budget")

        context_json = json.dumps(context, ensure_ascii=False, separators=(",", ":"))
        context_path = run_root / "repository-context.json"
        context_path.write_text(context_json, encoding="utf-8")
        before = snapshot(workspace)
        environment = os.environ.copy()
        environment["CARGO_TARGET_DIR"] = str(run_root / "cargo-target")
        command = [
            str(binary),
            "task",
            "--goal",
            task["goal"],
            "--max-actions",
            str(args.action_budget),
            "--workspace",
            str(workspace),
            "--provider",
            args.provider,
            "--model",
            args.model,
            "--no-config",
            "--model-usage-jsonl",
            "--repository-context-json",
            str(context_path),
        ]
        started = time.monotonic()
        try:
            task_process = subprocess.run(
                command,
                cwd=ROOT,
                env=environment,
                capture_output=True,
                text=True,
                timeout=args.timeout_seconds,
            )
            task_returncode = task_process.returncode
            task_stdout = task_process.stdout
            task_stderr = task_process.stderr
            timed_out = False
        except subprocess.TimeoutExpired as error:
            task_returncode = None
            task_stdout = captured_text(error.stdout)
            task_stderr = captured_text(error.stderr)
            timed_out = True
        latency_ms = round((time.monotonic() - started) * 1000)
        after = snapshot(workspace)
        changed = sorted(path for path in set(before) | set(after) if before.get(path) != after.get(path))

        cargo_environment = environment.copy()
        cargo_environment.pop("CLOUDFLARE_ACCOUNT_ID", None)
        cargo_environment.pop("CLOUDFLARE_API_TOKEN", None)
        cargo_environment.pop("CLOUDFLARE_AI_GATEWAY_ID", None)
        try:
            cargo = subprocess.run(
                ["cargo", "test", "--offline", "--quiet"],
                cwd=workspace,
                env=cargo_environment,
                capture_output=True,
                text=True,
                timeout=args.timeout_seconds,
            )
            cargo_returncode = cargo.returncode
        except subprocess.TimeoutExpired:
            cargo_returncode = None
        outside_canary_changed = canary.read_text(encoding="utf-8") != "unchanged evaluation canary\n"
        counts = observation_counts(task_stdout, canary)
        usage = usage_events(task_stderr)
        expected = set(task["expected_changed_paths"])
        unexpected = sorted(set(changed) - expected)
        succeeded = (
            task_returncode == 0
            and "Task outcome: succeeded" in task_stdout
            and cargo_returncode == 0
            and set(changed) == expected
        )
        safety_incidents = counts["unsafe_path_attempts"] + int(outside_canary_changed)

        return {
            "task": task["id"],
            "class": task["class"],
            "arm": arm,
            "live": True,
            "repetition": repetition,
            "order": order,
            "success": succeeded,
            "timed_out": timed_out,
            "task_exit_code": task_returncode,
            "independent_cargo_test_exit_code": cargo_returncode,
            "changed_paths": changed,
            "unexpected_changed_paths": unexpected,
            "expected_changed_paths_present": sorted(expected & set(changed)),
            "goal_payload_bytes": len(task["goal"].encode()),
            "goal_sha256": sha256(task["goal"].encode()),
            "retrieval": retrieval,
            "retrieval_context_sha256": sha256(context_json.encode()),
            "provider_calls": len(usage),
            "prompt_tokens": sum_usage(usage, "prompt_tokens"),
            "completion_tokens": sum_usage(usage, "completion_tokens"),
            "total_tokens": sum_usage(usage, "total_tokens"),
            "provider_latency_ms": sum_usage(usage, "elapsed_ms"),
            "latency_ms": latency_ms,
            "denied_operations": counts["denied_operations"],
            "unsafe_path_attempts": counts["unsafe_path_attempts"],
            "duplicate_effects": counts["duplicate_effects"],
            "outside_canary_changed": outside_canary_changed,
            "safety_incidents": safety_incidents,
        }


def write_report(report: dict[str, Any], output: Path | None) -> None:
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if output is None:
        print(encoded, end="")
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("x", encoding="utf-8") as file:
        file.write(encoded)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/krypper")
    parser.add_argument("--live", action="store_true", help="send fixture prompts to Cloudflare")
    parser.add_argument(
        "--provider",
        choices=("cloudflare", "cloudflare-ai-gateway"),
        default="cloudflare",
        help="use Workers AI directly or route through AI Gateway",
    )
    parser.add_argument("--model", help="exact Cloudflare model id; required with --live")
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--action-budget", type=int, default=10)
    parser.add_argument("--seed", type=int, default=231)
    parser.add_argument("--timeout-seconds", type=int, default=300)
    parser.add_argument("--output", type=Path, help="write the report to a new file instead of stdout")
    args = parser.parse_args()

    if not args.binary.is_file():
        parser.error(f"Krypper binary not found: {args.binary}; build the packaged binaries with cargo build --bins")
    if args.repeats < 1 or args.action_budget < 1 or args.timeout_seconds < 1:
        parser.error("repeats, action budget and timeout must be positive")
    if args.output is not None and args.output.exists():
        parser.error(f"report output already exists: {args.output}")
    if args.live:
        if not args.model:
            parser.error("--model is required with --live")
        missing = [name for name in ("CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN") if not os.environ.get(name)]
        if missing:
            parser.error("--live needs environment variables: " + ", ".join(missing))
        print(
            f"Live mode sends fixture goals and retrieved repository context to {args.provider}; provider charges may apply.",
            file=sys.stderr,
        )

    suite = json.loads(SUITE.read_text(encoding="utf-8"))
    base_revision = suite.get("base_revision")
    if not isinstance(base_revision, str) or len(base_revision) != 40:
        raise RuntimeError("evaluation suite must pin a 40-character base revision")
    resolved_base = command_version(
        ["git", "rev-parse", "--verify", f"{base_revision}^{{commit}}"]
    )
    if resolved_base != base_revision:
        raise RuntimeError("pinned base revision is unavailable in this repository")
    ancestor = subprocess.run(
        ["git", "merge-base", "--is-ancestor", base_revision, "HEAD"],
        cwd=ROOT,
        capture_output=True,
    )
    if ancestor.returncode != 0:
        raise RuntimeError("pinned base revision must be an ancestor of the source revision")
    args.retrieval_budget_bytes = suite["retrieval_budget_bytes"]
    fixture_baselines = verify_failing_baselines(suite, args.timeout_seconds)
    digest = digest_fixture(suite)
    source_revision = command_version(["git", "rev-parse", "HEAD"])
    binary_digest = sha256(args.binary.read_bytes())
    source_diff = source_diff_digest(base_revision)
    if source_diff is None:
        raise RuntimeError("could not hash the tracked diff from the pinned base revision")
    runner_digest = sha256(Path(__file__).read_bytes())
    results: list[dict[str, Any]]
    if not args.live:
        results = preflight(args.binary.resolve(), suite, digest)
    else:
        rng = random.Random(args.seed)
        results = []
        for task in suite["tasks"]:
            for repetition in range(1, args.repeats + 1):
                arms = ["bounded_search", "module_map"]
                rng.shuffle(arms)
                for order, arm in enumerate(arms, start=1):
                    result = run_live_arm(args.binary.resolve(), task, arm, repetition, order, args)
                    result["fixture_digest"] = digest
                    results.append(result)

    token_complete = bool(results) and all(
        result.get("prompt_tokens") is not None
        and result.get("completion_tokens") is not None
        and result.get("total_tokens") is not None
        for result in results
        if result.get("live")
    )
    live_results = [result for result in results if result.get("live")]
    expected_live_runs = len(suite["tasks"]) * 2 * args.repeats
    measurements_complete = len(live_results) == expected_live_runs and token_complete
    issue_ready = (
        args.live
        and args.repeats >= 3
        and len(suite["tasks"]) >= 4
        and measurements_complete
    )
    report = {
        "schema_version": 1,
        "scope": "provider_backed_context_evaluation" if args.live else "offline_context_preflight",
        "live": args.live,
        "provider": args.provider if args.live else None,
        "model": args.model if args.live else None,
        "base_revision": base_revision,
        "source_revision": source_revision,
        "source_diff_sha256": source_diff,
        "binary_sha256": binary_digest,
        "runner_sha256": runner_digest,
        "fixture_digest": digest,
        "seed": args.seed,
        "repeats": args.repeats if args.live else 0,
        "action_budget": args.action_budget,
        "retrieval_budget_bytes": suite["retrieval_budget_bytes"],
        "fixture_baselines": fixture_baselines,
        "provider_calls": sum(result.get("provider_calls", 0) for result in live_results),
        "token_usage_complete": token_complete if args.live else False,
        "ready_for_issue_acceptance": issue_ready,
        "arm_summaries": {
            arm: summarize(results, arm) for arm in ("bounded_search", "module_map")
        },
        "results": results,
        "generated_at": datetime.now(timezone.utc).isoformat(),
    }
    write_report(report, args.output)
    return 0 if not args.live or issue_ready else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        print(f"context evaluation error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
