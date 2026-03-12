# RSDB Agent Instructions

This file is the repo-local source of truth for agent release behavior.

## GitHub Release

When asked to publish, refresh, or update the RSDB GitHub release:

1. Run `./scripts/release-rsdb.sh` from the repo root.
2. Do not reconstruct the release workflow manually unless the user explicitly asks for a custom flow or the script itself needs to be repaired.
3. After the script finishes, report:
   - whether the release was updated or created
   - the commit used for the release
   - the uploaded assets
   - any failure and the next corrective action

If the script fails on a precondition, surface the exact error instead of guessing around it.
