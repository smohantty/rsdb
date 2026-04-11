# RSDB Tech Debt Tracker

Snapshot date: 2026-04-11

## Open Debt

- Security model is still `none`.
  `rsdbd` advertises no authentication and no transport encryption, so the
  daemon should be treated as trusted-network-only until that changes.
- No checked-in CI workflow exists.
  All verification is currently driven by local commands and shell scripts.
- No checked-in doc freshness enforcement exists.
  The structured docs are present now, but there is no automated check that
  catches stale statements or broken links.
- Workspace and RPM versioning still require manual synchronization.
  The workspace version lives in `Cargo.toml`, while the RPM spec also carries a
  `Version:` field.
- The host CLI and daemon each currently concentrate most behavior in a single
  `main.rs`.
  That keeps navigation simple today, but future refactors should consider
  extracting focused modules before the maintenance surface grows much further.
