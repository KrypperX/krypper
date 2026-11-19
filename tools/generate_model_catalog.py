#!/usr/bin/env python3
"""Deterministically normalize the vendored models.dev snapshot (no network)."""
import argparse
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "snapshots/models-dev/api.json"
META = ROOT / "snapshots/models-dev/metadata.json"
TARGET = ROOT / "src/catalog_data.json"


def normalize(raw):
    providers = []
    for provider_id, provider in sorted(raw.items()):
        if not isinstance(provider, dict):
            continue
        models = provider.get("models", {})
        if not isinstance(models, dict):
            continue
        for model_id, model in sorted(models.items()):
            if not isinstance(model, dict):
                continue
            limits = model.get("limit") or {}
            if not isinstance(limits, dict):
                limits = {}
            providers.append({
                "provider": provider_id,
                "id": model_id,
                "name": model.get("name") or model_id,
                "context_window": limits.get("context"),
                "max_output_tokens": limits.get("output"),
                "tool_calling": model.get("tool_call"),
                "reasoning": model.get("reasoning"),
                "modalities": model.get("modalities"),
                "status": model.get("status"),
            })
    return providers


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="verify committed output")
    args = parser.parse_args()
    source = SOURCE.read_bytes()
    metadata = json.loads(META.read_text())
    if hashlib.sha256(source).hexdigest() != metadata["sha256"]:
        raise SystemExit("Snapshot SHA-256 mismatch")
    raw = json.loads(source)
    if not isinstance(raw, dict) or len(raw) != metadata["provider_count"]:
        raise SystemExit("Snapshot provider count mismatch")
    models = normalize(raw)
    if not models:
        raise SystemExit("No models found; verify upstream schema")
    output = (json.dumps(models, ensure_ascii=False, indent=2) + "\n").encode()
    if args.check:
        if not TARGET.exists() or TARGET.read_bytes() != output:
            raise SystemExit("Catalog out of date: rerun tools/generate_model_catalog.py")
        print(f"Verified {len(models)} model records")
    else:
        TARGET.write_bytes(output)
        print(f"Generated {len(models)} model records: {TARGET.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
