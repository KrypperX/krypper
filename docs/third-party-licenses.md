# Third-party licensing and attribution

Krypper's own source is offered under Apache-2.0; see the root [LICENSE](../LICENSE) and the package's `Cargo.toml` license field. This declaration does **not** relicense third-party components or upstream data.

## Checked-in upstream data

- `snapshots/models-dev/api.json` is a vendored snapshot of [models.dev](https://models.dev), used as offline model metadata. The upstream MIT copyright and permission notice is preserved verbatim at [`snapshots/models-dev/LICENSE`](../snapshots/models-dev/LICENSE); [`metadata.json`](../snapshots/models-dev/metadata.json) records provenance and checksum. Keep these files together when distributing the snapshot.
- The MIT terms apply to the upstream snapshot, not automatically to Krypper's original Rust code. Apache-2.0 and MIT can coexist with their respective notices retained.

## Rust dependencies

`Cargo.toml` directly names `ignore`, `libc`, `ring`, `rpassword`, `serde`, `serde_json`, `signal-hook`, `toml`, `ureq`, and `uuid`; `proptest` is a development dependency. `Cargo.lock` pins the dependency graph. Each crate retains its own licensing; the project's Apache-2.0 declaration does not override it.

`deny.toml` defines the automated dependency policy. The CI check examines the locked graph with all features, development dependencies, and the Linux, macOS, and Windows targets listed there. It rejects unknown licenses, dependency sources, advisories, yanked crates, and wildcard requirements. The current policy accepts Apache-2.0, BSD-3-Clause, CDLA-Permissive-2.0, ISC, MIT, Unicode-3.0, and Unlicense terms. Duplicate dependency versions are warnings so compatibility-driven pairs remain visible for review.

The license decision relies on crate-supplied metadata when available; the check cannot verify that metadata against every license or notice in the package contents. It also does not verify project-code ownership, contributor authorization, every bundled source file's notice, or the notice bundle for a specific distribution. Do not treat a passing check as legal clearance.

Crates.io publication is intentionally disabled in `Cargo.toml` (`publish = false`). There is no automated release workflow because no release artifact or distribution policy has been approved. Revisit release automation after the intended artifacts, signing, and notice bundle are defined and reviewed.

The Linux coverage workflow reports line coverage for all Cargo targets on pull requests and pushes to `main`. It is report-only: no threshold is enforced until a stable baseline and target are reviewed. The report does not include doctests or branch coverage.

## Distribution checklist

- [x] Root Apache-2.0 license text present and package metadata declared.
- [x] Vendored models.dev MIT license retained at its source location.
- [x] README identifies the project license and third-party exception.
- [x] Automated license, advisory, dependency-source, wildcard, and duplicate-version checks cover the configured locked Cargo graph.
- [ ] Inspect bundled source/asset notices and retain required notices in each intended release artifact.
- [ ] Verify ownership or contributor authorization for all project code before release.
- [ ] Review third-party notice and redistribution obligations for each intended release artifact.
