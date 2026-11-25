# Changelog

Notable user-visible changes are recorded here. Keep implementation details and
unverified quality claims out of release notes.

## Unreleased

### Added

- Offline provider and model catalog metadata with explicit refresh and
  provenance.
- Opt-in automatic model routing and bounded provider fallback.
- Bounded repository context retrieval and an opt-in Rust module map.
- Safe multi-file impact analysis with stale-content checks and bounded repair
  verification.

### Changed

- Provider setup and model selection are stored in per-user configuration, with
  explicit routing and fallback choices.

### Security

- Workspace edits remain confined to the existing capability paths. Sandboxed
  Cargo tests require the verified Linux backend; unsupported platform
  capabilities remain explicitly unavailable.
