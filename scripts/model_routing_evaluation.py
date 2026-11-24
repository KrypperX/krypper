#!/usr/bin/env python3
"""Offline-first evaluation of manual and automatic Krypper model routing."""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation, ROUND_CEILING
from pathlib import Path
from typing import Any

import context_model_evaluation as context_eval


ROOT = context_eval.ROOT
SUITE = context_eval.SUITE
FIXTURES = context_eval.FIXTURES
REQUIRED_CATEGORIES = {"coding", "reasoning", "tool_use"}
PUBLIC_SETTINGS_FILES = (
    "model-evidence.json",
    "model-evidence.key",
    "model-catalog-cache.json",
)
PROVIDER_CREDENTIALS = (
    "OPENAI_API_KEY",
    "XAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_WORKSPACE_ID",
    "GEMINI_API_KEY",
    "GROQ_API_KEY",
    "OPENROUTER_API_KEY",
    "DEEPSEEK_API_KEY",
    "ZAI_API_KEY",
    "MISTRAL_API_KEY",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_API_TOKEN",
    "CLOUDFLARE_AI_GATEWAY_ID",
)
COMPATIBLE_PROVIDERS = {
    "openai",
    "xai",
    "cloudflare",
    "cloudflare-ai-gateway",
    "groq",
    "openrouter",
    "deepseek",
    "zai",
    "mistral",
}
PROVIDER_REQUIRED_CREDENTIALS = {
    "openai": ("OPENAI_API_KEY",),
    "xai": ("XAI_API_KEY",),
    "cloudflare": ("CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN"),
    "cloudflare-ai-gateway": ("CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN"),
    "groq": ("GROQ_API_KEY",),
    "openrouter": ("OPENROUTER_API_KEY",),
    "deepseek": ("DEEPSEEK_API_KEY",),
    "zai": ("ZAI_API_KEY",),
    "mistral": ("MISTRAL_API_KEY",),
}
ROUTE_LINE = re.compile(
    r"^krypper: selected route ([^/\s]+)/(.+?) \(([^()]*)\)$"
)


def environment_without_provider_credentials(
    base_environment: dict[str, str] | None = None,
) -> dict[str, str]:
    environment = (os.environ if base_environment is None else base_environment).copy()
    for name in PROVIDER_CREDENTIALS:
        environment.pop(name, None)
    environment.pop("KRYPPER_CONFIG", None)
    return environment


def environment_for_provider(
    provider: str, base_environment: dict[str, str]
) -> dict[str, str]:
    required = set(PROVIDER_REQUIRED_CREDENTIALS.get(provider, ()))
    if provider == "cloudflare-ai-gateway" and base_environment.get("CLOUDFLARE_AI_GATEWAY_ID"):
        required.add("CLOUDFLARE_AI_GATEWAY_ID")
    environment = base_environment.copy()
    for name in PROVIDER_CREDENTIALS:
        if name not in required:
            environment.pop(name, None)
    environment.pop("KRYPPER_CONFIG", None)
    return environment


def parse_selected_route(stderr: str) -> dict[str, str] | None:
    for line in stderr.splitlines():
        match = ROUTE_LINE.fullmatch(line)
        if match:
            return {
                "provider": match.group(1),
                "model": match.group(2),
                "reason": match.group(3),
            }
    return None


def decimal_value(value: Any) -> Decimal | None:
    if isinstance(value, bool) or value is None:
        return None
    try:
        parsed = Decimal(str(value))
    except (InvalidOperation, ValueError):
        return None
    return parsed if parsed.is_finite() and parsed >= 0 else None


def estimate_cost_micro_usd(
    events: list[dict[str, Any]], pricing: dict[str, Any] | None
) -> int | None:
    if not events or not isinstance(pricing, dict):
        return None
    input_rate = decimal_value(pricing.get("input_usd_per_million_tokens"))
    output_rate = decimal_value(pricing.get("output_usd_per_million_tokens"))
    if input_rate is None or output_rate is None:
        return None

    micro_usd = Decimal(0)
    for event in events:
        input_tokens = event.get("prompt_tokens")
        output_tokens = event.get("completion_tokens")
        if (
            not isinstance(input_tokens, int)
            or isinstance(input_tokens, bool)
            or input_tokens < 0
            or not isinstance(output_tokens, int)
            or isinstance(output_tokens, bool)
            or output_tokens < 0
        ):
            return None
        micro_usd += input_rate * input_tokens + output_rate * output_tokens
    return int(micro_usd.to_integral_value(rounding=ROUND_CEILING))


def conservative_request_cost_micro_usd(
    model: dict[str, Any], pricing: dict[str, Any] | None
) -> int | None:
    if not isinstance(pricing, dict):
        return None
    context_window = model.get("context_window")
    max_output_tokens = model.get("max_output_tokens")
    input_rate = decimal_value(pricing.get("input_usd_per_million_tokens"))
    output_rate = decimal_value(pricing.get("output_usd_per_million_tokens"))
    if (
        not isinstance(context_window, int)
        or isinstance(context_window, bool)
        or context_window < 0
        or not isinstance(max_output_tokens, int)
        or isinstance(max_output_tokens, bool)
        or max_output_tokens < 0
        or input_rate is None
        or output_rate is None
    ):
        return None
    cost = input_rate * context_window + output_rate * max_output_tokens
    return int(cost.to_integral_value(rounding=ROUND_CEILING))


def config_contents(provider: str, model: str, routing_mode: str) -> str:
    if routing_mode not in ("manual", "automatic"):
        raise ValueError("routing mode must be manual or automatic")
    return (
        f"provider = {json.dumps(provider)}\n"
        f"model = {json.dumps(model)}\n"
        f"routing_mode = {json.dumps(routing_mode)}\n"
    )


def copy_public_settings(source: Path, destination: Path) -> list[str]:
    destination.mkdir(parents=True, exist_ok=True, mode=0o700)
    copied = []
    for name in sorted(PUBLIC_SETTINGS_FILES):
        source_path = source / name
        try:
            metadata = source_path.lstat()
        except FileNotFoundError:
            continue
        if not source_path.is_file() or source_path.is_symlink():
            raise RuntimeError(f"provider evidence file is not a regular file: {name}")
        if metadata.st_size > 64 * 1024 * 1024:
            raise RuntimeError(f"provider evidence file exceeds the copy limit: {name}")
        destination_path = destination / name
        destination_path.write_bytes(source_path.read_bytes())
        try:
            destination_path.chmod(0o600)
        except OSError:
            pass
        copied.append(name)
    return copied


def source_settings_directory(environment: dict[str, str]) -> Path:
    xdg = environment.get("XDG_CONFIG_HOME")
    if xdg and Path(xdg).is_absolute():
        return Path(xdg) / "krypper"
    home = environment.get("HOME") or environment.get("USERPROFILE")
    if home and Path(home).is_absolute():
        return Path(home) / ".config" / "krypper"
    raise RuntimeError("set an absolute XDG_CONFIG_HOME or HOME to locate provider evidence")


def arm_environment(
    base_environment: dict[str, str],
    source_settings: Path,
    xdg_config_home: Path,
    provider: str,
    model: str,
    routing_mode: str,
) -> tuple[dict[str, str], list[str]]:
    settings_directory = xdg_config_home / "krypper"
    copied = copy_public_settings(source_settings, settings_directory)
    config_path = settings_directory / "config.toml"
    config_path.write_text(config_contents(provider, model, routing_mode), encoding="utf-8")
    try:
        config_path.chmod(0o600)
    except OSError:
        pass
    environment = base_environment.copy()
    environment.pop("KRYPPER_CONFIG", None)
    environment["XDG_CONFIG_HOME"] = str(xdg_config_home)
    return environment, copied


def run_json_command(
    binary: Path, arguments: list[str], environment: dict[str, str], timeout_seconds: int
) -> dict[str, Any]:
    try:
        result = subprocess.run(
            [str(binary), *arguments],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("local Krypper preflight command failed") from error
    if result.returncode != 0:
        raise RuntimeError("local Krypper preflight command failed")
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError("local Krypper preflight returned invalid JSON") from error
    if not isinstance(value, dict):
        raise RuntimeError("local Krypper preflight returned an unexpected JSON shape")
    return value


def route_from_status(value: dict[str, Any]) -> dict[str, str] | None:
    route = value.get("route")
    if not isinstance(route, dict):
        return None
    provider = route.get("provider")
    model = route.get("model")
    reason = route.get("reason")
    if not all(isinstance(field, str) and field for field in (provider, model, reason)):
        return None
    return {"provider": provider, "model": model, "reason": reason}


def model_metadata(
    binary: Path,
    route: dict[str, str],
    environment: dict[str, str],
    timeout_seconds: int,
) -> dict[str, Any]:
    value = run_json_command(
        binary,
        [
            "models",
            "--provider",
            route["provider"],
            "--id",
            route["model"],
            "--json",
            "--provenance",
        ],
        environment,
        timeout_seconds,
    )
    models = value.get("models")
    if not isinstance(models, list):
        raise RuntimeError("model catalog preflight returned an unexpected JSON shape")
    matches = [entry for entry in models if isinstance(entry, dict) and entry.get("id") == route["model"]]
    if not matches:
        return {"pricing": None, "model": {}, "sources": [], "catalog_stale": None}
    entry = matches[0]
    return {
        "pricing": entry.get("pricing"),
        "model": {
            "context_window": entry.get("context_window"),
            "max_output_tokens": entry.get("max_output_tokens"),
        },
        "sources": entry.get("sources", []),
        "catalog_stale": value.get("catalog", {}).get("stale"),
        "catalog_origin": value.get("catalog", {}).get("origin"),
    }


def validate_provider_credentials(provider: str, environment: dict[str, str]) -> None:
    if provider not in COMPATIBLE_PROVIDERS:
        raise RuntimeError(
            "live routing evaluation requires a built-in OpenAI-compatible provider with usage telemetry"
        )
    missing = [
        name
        for name in PROVIDER_REQUIRED_CREDENTIALS[provider]
        if not environment.get(name)
    ]
    if missing:
        raise RuntimeError("live evaluation needs environment variables: " + ", ".join(missing))


def validate_pinned_revision(base_revision: str, environment: dict[str, str]) -> str:
    if not isinstance(base_revision, str) or len(base_revision) != 40:
        raise RuntimeError("evaluation suite must pin a 40-character base revision")
    resolved = context_eval.command_version(
        ["git", "rev-parse", "--verify", f"{base_revision}^{{commit}}"],
        base_environment=environment,
    )
    if resolved is None or resolved != base_revision:
        raise RuntimeError("pinned evaluation revision is unavailable")
    try:
        subprocess.run(
            ["git", "merge-base", "--is-ancestor", base_revision, "HEAD"],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise RuntimeError("pinned evaluation revision must be an ancestor of the source revision") from error
    revision = context_eval.command_version(
        ["git", "rev-parse", "HEAD"], base_environment=environment
    )
    if revision is None:
        raise RuntimeError("could not read the source revision")
    return revision


def verify_categories(suite: dict[str, Any]) -> None:
    actual = {
        category
        for task in suite.get("tasks", [])
        for category in task.get("categories", [])
        if isinstance(category, str)
    }
    if not REQUIRED_CATEGORIES.issubset(actual):
        missing = ", ".join(sorted(REQUIRED_CATEGORIES - actual))
        raise RuntimeError(f"evaluation fixture suite is missing task categories: {missing}")


def preflight_arm_routes(
    binary: Path,
    source_settings: Path,
    temporary: Path,
    provider: str,
    model: str,
    base_environment: dict[str, str],
    timeout_seconds: int,
) -> tuple[dict[str, Any], dict[str, dict[str, str]], dict[str, dict[str, Any]], dict[str, list[str]]]:
    arm_environments: dict[str, dict[str, str]] = {}
    routes: dict[str, dict[str, str]] = {}
    metadata: dict[str, dict[str, Any]] = {}
    copied_files: dict[str, list[str]] = {}
    for arm in ("manual", "automatic"):
        xdg = temporary / f"{arm}-xdg"
        environment, copied = arm_environment(
            base_environment,
            source_settings,
            xdg,
            provider,
            model,
            arm,
        )
        status = run_json_command(binary, ["setup", "--status", "--json"], environment, timeout_seconds)
        route = route_from_status(status)
        if route is None:
            if arm == "automatic":
                raise RuntimeError(
                    "automatic route is unavailable; provide fresh successful probe evidence for this provider and model"
                )
            raise RuntimeError("manual route could not be read from isolated settings")
        if route["provider"] not in COMPATIBLE_PROVIDERS:
            raise RuntimeError("selected route does not support provider usage telemetry")
        arm_environments[arm] = environment
        routes[arm] = route
        metadata[arm] = model_metadata(
            binary,
            route,
            environment_without_provider_credentials(environment),
            timeout_seconds,
        )
        copied_files[arm] = copied
    return (
        {"manual": routes["manual"], "automatic": routes["automatic"]},
        arm_environments,
        metadata,
        copied_files,
    )


def captured_text(value: str | bytes | None) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value or ""


def run_live_arm(
    binary: Path,
    task: dict[str, Any],
    arm: str,
    repetition: int,
    order: int,
    route: dict[str, str],
    route_metadata: dict[str, Any],
    environment: dict[str, str],
    run_root: Path,
    args: argparse.Namespace,
    fixture_digest: str,
    source_revision: str,
) -> dict[str, Any]:
    workspace = run_root / "workspace"
    shutil.copytree(FIXTURES / task["workspace"], workspace)
    canary = run_root / "outside-canary.txt"
    canary.write_text("unchanged evaluation canary\n", encoding="utf-8")
    context_environment = environment_without_provider_credentials(environment)
    context = context_eval.read_context(
        binary, workspace, task["goal"], "bounded_search", context_environment
    )
    context_bytes = context.get("bytes")
    if not isinstance(context_bytes, int) or context_bytes > args.context_budget_bytes:
        raise RuntimeError(f"{task['id']} exceeded the configured context budget")
    context_json = json.dumps(context, ensure_ascii=False, separators=(",", ":"))
    context_path = run_root / "repository-context.json"
    context_path.write_text(context_json, encoding="utf-8")

    before = context_eval.snapshot(workspace)
    run_environment = environment.copy()
    run_environment["CARGO_TARGET_DIR"] = str(run_root / "cargo-target")
    command = [
        str(binary),
        "task",
        "--goal",
        task["goal"],
        "--max-actions",
        str(args.action_budget),
        "--workspace",
        str(workspace),
        "--model-usage-jsonl",
        "--repository-context-json",
        str(context_path),
    ]
    started = time.monotonic()
    try:
        task_process = subprocess.run(
            command,
            cwd=ROOT,
            env=run_environment,
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
    except OSError:
        task_returncode = None
        task_stdout = ""
        task_stderr = ""
        timed_out = False
    elapsed_ms = round((time.monotonic() - started) * 1000)
    after = context_eval.snapshot(workspace)
    changed = sorted(path for path in set(before) | set(after) if before.get(path) != after.get(path))

    cargo_environment = environment_without_provider_credentials(run_environment)
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
    except (OSError, subprocess.TimeoutExpired):
        cargo_returncode = None

    outside_canary_changed = canary.read_text(encoding="utf-8") != "unchanged evaluation canary\n"
    observation_counts = context_eval.observation_counts(task_stdout, canary)
    usage = context_eval.usage_events(task_stderr)
    observed_route = parse_selected_route(task_stderr) if arm == "automatic" else route
    if arm == "automatic" and observed_route is None:
        observed_route = None
    observed_metadata = route_metadata
    if observed_route is not None and (
        observed_route["provider"], observed_route["model"]
    ) != (route["provider"], route["model"]):
        observed_metadata = model_metadata(
            binary,
            observed_route,
            environment_without_provider_credentials(environment),
            args.timeout_seconds,
        )

    expected_paths = set(task["expected_changed_paths"])
    unexpected_paths = sorted(set(changed) - expected_paths)
    success = (
        task_returncode == 0
        and "Task outcome: succeeded" in task_stdout
        and cargo_returncode == 0
        and set(changed) == expected_paths
    )
    safety_incidents = observation_counts["unsafe_path_attempts"] + int(outside_canary_changed)
    token_usage_cost = None
    reserved_estimate = None
    if observed_metadata.get("catalog_stale") is False:
        token_usage_cost = estimate_cost_micro_usd(usage, observed_metadata.get("pricing"))
        request_estimate = conservative_request_cost_micro_usd(
            observed_metadata.get("model", {}), observed_metadata.get("pricing")
        )
        if request_estimate is not None and usage:
            reserved_estimate = request_estimate * len(usage)
    route_matches_preflight = observed_route is not None and (
        observed_route["provider"], observed_route["model"]
    ) == (route["provider"], route["model"])

    return {
        "task": task["id"],
        "class": task["class"],
        "categories": task["categories"],
        "arm": arm,
        "live": True,
        "repetition": repetition,
        "order": order,
        "selected_route": observed_route,
        "route_source": "task_stderr" if arm == "automatic" else "isolated_manual_config",
        "route_matches_preflight": route_matches_preflight,
        "success": success,
        "timed_out": timed_out,
        "task_exit_code": task_returncode,
        "independent_cargo_test_exit_code": cargo_returncode,
        "changed_paths": changed,
        "unexpected_changed_paths": unexpected_paths,
        "expected_changed_paths_present": sorted(expected_paths & set(changed)),
        "fixture_digest": fixture_digest,
        "source_revision": source_revision,
        "retrieval_context_sha256": context_eval.sha256(context_json.encode()),
        "model_decisions": len(usage),
        "prompt_tokens": context_eval.sum_usage(usage, "prompt_tokens"),
        "completion_tokens": context_eval.sum_usage(usage, "completion_tokens"),
        "total_tokens": context_eval.sum_usage(usage, "total_tokens"),
        "model_decision_latency_ms": context_eval.sum_usage(usage, "elapsed_ms"),
        "end_to_end_latency_ms": elapsed_ms,
        "token_usage_cost_estimate_micro_usd": token_usage_cost,
        "router_reserved_estimate_micro_usd": reserved_estimate,
        "actual_billed_cost_usd": None,
        "price_catalog_stale": observed_metadata.get("catalog_stale"),
        "price_sources": observed_metadata.get("sources", []),
        "denied_operations": observation_counts["denied_operations"],
        "unsafe_path_attempts": observation_counts["unsafe_path_attempts"],
        "duplicate_effects": observation_counts["duplicate_effects"],
        "outside_canary_changed": outside_canary_changed,
        "safety_incidents": safety_incidents,
    }


def sum_complete(results: list[dict[str, Any]], field: str) -> int | None:
    values = [result.get(field) for result in results]
    if not values or any(not isinstance(value, int) or value < 0 for value in values):
        return None
    return sum(values)


def summarize(results: list[dict[str, Any]], arm: str) -> dict[str, Any]:
    runs = [result for result in results if result["arm"] == arm and result.get("live")]
    if not runs:
        return {"runs": 0}
    successes = sum(bool(run["success"]) for run in runs)
    latencies = sorted(run["end_to_end_latency_ms"] for run in runs)
    estimates = [run.get("token_usage_cost_estimate_micro_usd") for run in runs]
    reserved = [run.get("router_reserved_estimate_micro_usd") for run in runs]
    known_estimates = [value for value in estimates if isinstance(value, int)]
    known_reserved = [value for value in reserved if isinstance(value, int)]
    return {
        "runs": len(runs),
        "task_successes": successes,
        "task_success_rate": successes / len(runs),
        "task_success_wilson_95": context_eval.wilson_95(successes, len(runs)),
        "model_decisions": sum(run.get("model_decisions", 0) for run in runs),
        "prompt_tokens": sum_complete(runs, "prompt_tokens"),
        "completion_tokens": sum_complete(runs, "completion_tokens"),
        "total_tokens": sum_complete(runs, "total_tokens"),
        "model_decision_latency_ms": sum_complete(runs, "model_decision_latency_ms"),
        "end_to_end_latency_ms_p50": statistics.median(latencies),
        "end_to_end_latency_ms_p95": latencies[max(0, math.ceil(0.95 * len(latencies)) - 1)],
        "token_usage_cost_estimate_micro_usd": (
            sum(known_estimates) if len(known_estimates) == len(runs) else None
        ),
        "token_usage_cost_estimate_coverage": len(known_estimates),
        "router_reserved_estimate_micro_usd": (
            sum(known_reserved) if len(known_reserved) == len(runs) else None
        ),
        "router_reserved_estimate_coverage": len(known_reserved),
        "actual_billed_cost_usd": None,
        "actual_billed_cost_status": "not reported by model usage telemetry",
        "safety_incidents": sum(run["safety_incidents"] for run in runs),
        "duplicate_effects": sum(run["duplicate_effects"] for run in runs),
    }


def paired_outcomes(results: list[dict[str, Any]]) -> dict[str, int]:
    by_pair: dict[tuple[str, int], dict[str, bool]] = {}
    for result in results:
        if not result.get("live"):
            continue
        key = (result["task"], result["repetition"])
        by_pair.setdefault(key, {})[result["arm"]] = bool(result["success"])
    counts = {"manual_wins": 0, "automatic_wins": 0, "ties": 0, "complete_pairs": 0}
    for pair in by_pair.values():
        if set(pair) != {"manual", "automatic"}:
            continue
        counts["complete_pairs"] += 1
        if pair["manual"] == pair["automatic"]:
            counts["ties"] += 1
        elif pair["manual"]:
            counts["manual_wins"] += 1
        else:
            counts["automatic_wins"] += 1
    return counts


def category_summaries(results: list[dict[str, Any]]) -> dict[str, dict[str, dict[str, Any]]]:
    return {
        category: {
            arm: summarize(
                [run for run in results if category in run.get("categories", [])], arm
            )
            for arm in ("manual", "automatic")
        }
        for category in sorted(REQUIRED_CATEGORIES)
    }


def sample_readiness(
    results: list[dict[str, Any]],
    expected_runs: int,
    repeats: int,
    task_count: int,
    routes: dict[str, dict[str, str]],
) -> dict[str, bool]:
    actual_runs = [result for result in results if result.get("live")]
    token_usage_complete = bool(actual_runs) and all(
        result.get("model_decisions", 0) > 0
        and result.get("prompt_tokens") is not None
        and result.get("completion_tokens") is not None
        and result.get("total_tokens") is not None
        and result.get("model_decision_latency_ms") is not None
        for result in actual_runs
    )
    complete = len(actual_runs) == expected_runs
    independently_verified = all(
        isinstance(result.get("independent_cargo_test_exit_code"), int)
        for result in actual_runs
    )
    route_trace_complete = all(
        result.get("selected_route") is not None and result.get("route_matches_preflight")
        for result in actual_runs
    )
    route_match = (
        routes["manual"]["provider"], routes["manual"]["model"]
    ) == (routes["automatic"]["provider"], routes["automatic"]["model"])
    ready = (
        repeats >= 3
        and task_count >= 4
        and complete
        and independently_verified
        and token_usage_complete
        and route_trace_complete
    )
    return {
        "token_usage_complete": token_usage_complete,
        "sample_complete": complete,
        "independent_outcomes_complete": independently_verified,
        "route_trace_complete": route_trace_complete,
        "route_match": route_match,
        "ready_for_issue_acceptance": ready,
        "matched_model_comparison_ready": ready and route_match,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/krypper")
    parser.add_argument("--live", action="store_true", help="send fixture tasks to the configured provider")
    parser.add_argument("--provider", default="cloudflare", help="manual route anchor provider")
    parser.add_argument("--model", help="manual route anchor model ID; required with --live")
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--action-budget", type=int, default=10)
    parser.add_argument("--context-budget-bytes", type=int, default=4096)
    parser.add_argument("--seed", type=int, default=303)
    parser.add_argument("--timeout-seconds", type=int, default=300)
    parser.add_argument("--output", type=Path, help="write the report to a new file")
    args = parser.parse_args()

    if not args.binary.is_file():
        parser.error(f"Krypper binary not found: {args.binary}; build it with cargo build --bin krypper")
    if min(args.repeats, args.action_budget, args.context_budget_bytes, args.timeout_seconds) < 1:
        parser.error("repeats, action budget, context budget and timeout must be positive")
    if args.output is not None and args.output.exists():
        parser.error(f"report output already exists: {args.output}")
    if args.live and not args.model:
        parser.error("--model is required with --live")

    suite = json.loads(SUITE.read_text(encoding="utf-8"))
    verify_categories(suite)
    clean_environment = environment_without_provider_credentials()
    source_revision = validate_pinned_revision(suite.get("base_revision"), clean_environment)
    digest = context_eval.digest_fixture(suite)
    binary_digest = context_eval.sha256(args.binary.read_bytes())
    source_diff = context_eval.source_diff_digest(suite["base_revision"], clean_environment)
    if source_diff is None:
        raise RuntimeError("could not hash the tracked source diff")
    runner_digest = context_eval.sha256(Path(__file__).read_bytes())
    baselines = context_eval.verify_failing_baselines(
        suite, args.timeout_seconds, clean_environment
    )
    expected_runs = len(suite["tasks"]) * 2 * args.repeats
    report: dict[str, Any] = {
        "schema_version": 1,
        "scope": "manual_vs_automatic_routing" if args.live else "offline_routing_preflight",
        "live": args.live,
        "provider_anchor": args.provider if args.live else None,
        "manual_model_anchor": args.model if args.live else None,
        "base_revision": suite["base_revision"],
        "source_revision": source_revision,
        "source_diff_sha256": source_diff,
        "binary_sha256": binary_digest,
        "runner_sha256": runner_digest,
        "fixture_digest": digest,
        "seed": args.seed,
        "repeats": args.repeats if args.live else 0,
        "action_budget": args.action_budget,
        "context_budget_bytes": args.context_budget_bytes,
        "fixture_categories": sorted(REQUIRED_CATEGORIES),
        "fixture_baselines": baselines,
        "model_decisions": 0,
        "token_usage_complete": False,
        "actual_billed_cost_status": "not reported by model usage telemetry",
        "actual_billed_cost_usd": None,
        "ready_for_issue_acceptance": False,
        "matched_model_comparison_ready": False,
        "arm_summaries": {"manual": {"runs": 0}, "automatic": {"runs": 0}},
        "category_summaries": category_summaries([]),
        "paired_outcomes": {"manual_wins": 0, "automatic_wins": 0, "ties": 0, "complete_pairs": 0},
        "results": [],
        "limitations": [
            "The fixtures cover small Rust coding tasks, multi-file contract changes, reasoning about public fields, and tool use; they do not represent arbitrary repositories or standalone general reasoning.",
            "Provider usage telemetry gives token counts and model decision latency, not provider invoices. Token-based and router-reserved prices are estimates derived from the local model catalog.",
            "Catalog prices can differ from account, gateway, regional, cached-input, or negotiated billing rates; missing or stale prices remain unknown.",
            "Live results are observational and do not establish broad agent-quality superiority.",
        ],
        "generated_at": datetime.now(timezone.utc).isoformat(),
    }

    if args.live:
        validate_provider_credentials(args.provider, os.environ)
        provider_environment = environment_for_provider(args.provider, os.environ)
        print(
            "Live mode sends fixture goals and selected repository context to the configured provider; provider charges may apply.",
            file=sys.stderr,
        )
        source_settings = source_settings_directory(provider_environment)
        with tempfile.TemporaryDirectory(prefix="krypper-routing-evaluation-") as temporary:
            temporary_root = Path(temporary)
            routes, arm_environments, route_metadata, copied_files = preflight_arm_routes(
                args.binary.resolve(),
                source_settings,
                temporary_root,
                args.provider,
                args.model,
                provider_environment,
                args.timeout_seconds,
            )
            print(
                "Evaluation routes: "
                f"manual={routes['manual']['provider']}/{routes['manual']['model']}; "
                f"automatic={routes['automatic']['provider']}/{routes['automatic']['model']}",
                file=sys.stderr,
            )
            rng = random.Random(args.seed)
            results = []
            for task in suite["tasks"]:
                for repetition in range(1, args.repeats + 1):
                    arms = ["manual", "automatic"]
                    rng.shuffle(arms)
                    for order, arm in enumerate(arms, start=1):
                        run_root = temporary_root / f"run-{task['id']}-{repetition}-{arm}"
                        run_root.mkdir(mode=0o700)
                        result = run_live_arm(
                            args.binary.resolve(),
                            task,
                            arm,
                            repetition,
                            order,
                            routes[arm],
                            route_metadata[arm],
                            arm_environments[arm],
                            run_root,
                            args,
                            digest,
                            source_revision,
                        )
                        result["settings_files_copied"] = copied_files[arm]
                        results.append(result)

            readiness = sample_readiness(
                results, expected_runs, args.repeats, len(suite["tasks"]), routes
            )
            actual_runs = [result for result in results if result.get("live")]
            report.update(
                {
                    "routes": routes,
                    "settings_files_copied": copied_files,
                    "model_decisions": sum(
                        result.get("model_decisions", 0) for result in actual_runs
                    ),
                    **readiness,
                    "arm_summaries": {
                        arm: summarize(results, arm) for arm in ("manual", "automatic")
                    },
                    "category_summaries": category_summaries(results),
                    "paired_outcomes": paired_outcomes(results),
                    "results": results,
                }
            )
    else:
        report["planned_runs"] = expected_runs
        report["planned_arms"] = ["manual", "automatic"]
        report["ready_for_issue_acceptance"] = False

    context_eval.write_report(report, args.output)
    return 0 if not args.live or report["ready_for_issue_acceptance"] else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        print(f"model routing evaluation error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
