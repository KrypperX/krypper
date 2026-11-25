# Contributing

Krypper is a local-first Rust CLI and in-process agent runtime. Keep changes
focused on a user-visible problem and preserve the existing workspace, task,
provider, and permission boundaries.

## Before changing code

- Check the current product behavior, open issues and pull requests, and the
  relevant development notes for existing or overlapping functionality.
- Describe the user outcome, current behavior, and an acceptance condition that
  can be checked independently.
- Prefer a small change with focused regression coverage. Keep unverified
  behavior and limitations explicit in the pull request.
- Do not include API keys, tokens, prompts containing private data, or raw
  provider responses in source, tests, screenshots, or logs.

## Toolchain and validation

The minimum supported Rust version is `1.89.0` (`Cargo.toml` is the source of
truth). Before opening a pull request that changes Rust code, run:

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

The required GitHub checks are the final evidence for the exact pull request
head. The platform matrix runs the full suite on Linux and macOS; Windows is
compile-checked and does not run the platform tests. Catalog changes also need
the offline catalog check:

```sh
cargo run --locked --example generate_model_catalog -- --check
```

Host-only sandbox fixtures are intentionally ignored in ordinary test runs.
Run them only on a provisioned Linux host after checking the requirements in
[`docs/real-sandbox-fixtures.md`](docs/real-sandbox-fixtures.md), and report
which fixtures actually ran. Do not treat ignored tests or a local mock as
host-containment evidence. Live provider checks require explicit credentials;
never put those credentials or private request content in a pull request.

## Pull requests

- Link every pull request to its issue or issues and keep each one independently
  reviewable.
- Explain what changed, how it was verified, and what the evidence does not
  establish.
- Separate offline tests, live-provider results, host-only results, and
  unverified behavior.
- Wait for the required checks and independent review before merging. Close an
  issue only after its full acceptance criteria are satisfied or its remaining
  work has been transferred to a tracked follow-up.
