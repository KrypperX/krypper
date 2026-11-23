# Third-party licensing and attribution

Krypper's own source is offered under Apache-2.0; see the root [LICENSE](../LICENSE) and the package's `Cargo.toml` license field. This declaration does **not** relicense third-party components or upstream data.

## Checked-in upstream data

- `snapshots/models-dev/api.json` is a vendored snapshot of [models.dev](https://models.dev), used as offline model metadata. The upstream MIT copyright and permission notice is preserved verbatim at [`snapshots/models-dev/LICENSE`](../snapshots/models-dev/LICENSE); [`metadata.json`](../snapshots/models-dev/metadata.json) records provenance and checksum. Keep these files together when distributing the snapshot.
- The MIT terms apply to the upstream snapshot, not automatically to Krypper's original Rust code. Apache-2.0 and MIT can coexist with their respective notices retained.

## Rust dependencies

`Cargo.toml` directly names `ignore`, `libc`, `ring`, `rpassword`, `serde`, `serde_json`, `ureq`, and `uuid`. `Cargo.lock` pins transitive crates. Each crate retains its own upstream licensing; the project's Apache-2.0 declaration does not override it.

**Audit status:** This change checks the tracked upstream snapshot notice and the direct dependency manifest, but does not independently verify the license expression and notice requirements of every transitive crate, platform-specific dependency, bundled native code, or the provenance/rights of every contributed source file. Do not treat this note as a completed legal clearance.

Before a binary or source release, run a license inventory on the **exact locked dependency graph**, inspect any unknown/nonstandard licenses and bundled source, and retain applicable copyright and notice texts in the release distribution. For example, use a reviewed version of `cargo-license` or `cargo-deny` against `Cargo.lock`, and reconcile its output with actual crate manifests and source. Review project contribution provenance and any imported assets separately.

Crates.io publication is intentionally disabled in `Cargo.toml` (`publish = false`) while the product is still in development. Revisit publication only after product readiness and the required release review.

## Distribution checklist

- [x] Root Apache-2.0 license text present and package metadata declared.
- [x] Vendored models.dev MIT license retained at its source location.
- [x] README identifies the project license and third-party exception.
- [ ] Audit every locked direct/transitive dependency and bundled source/asset license.
- [ ] Verify ownership or contributor authorization for all project code before release.
- [ ] Review third-party notice and redistribution obligations for each intended release artifact.
