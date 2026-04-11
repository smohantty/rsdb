# RSDB Core Beliefs

Snapshot date: 2026-04-11

- Repository-local knowledge beats chat history.
  If a rule or workflow matters repeatedly, encode it in versioned docs or
  scripts.
- `AGENTS.md` is a map, not an encyclopedia.
  Keep it short and route into durable docs.
- Script entrypoints are part of the product surface.
  Release, development-device refresh, live regression, and loopback validation
  already exist as checked-in scripts and should be reused.
- The machine-facing contract is explicit.
  `rsdb agent schema` is the fastest summary of the autonomous interface and
  must stay aligned with code and docs.
- Protocol changes are lockstep changes.
  Do not change `rsdb-proto`, `rsdb-cli`, or `rsdbd` in isolation when the wire
  contract moves.
- State uncomfortable truths plainly.
  The current daemon security model is `none`; docs should say that directly
  instead of implying safety that does not exist.
- Prefer exact docs over aspirational docs.
  If a cleanup job, CI workflow, or linter is not checked in, document it as a
  gap rather than pretending it exists.
