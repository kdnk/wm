# Rift fork

- Use `but` for version-control writes. Work in the existing checkout on a dedicated branch; do not create worktrees.
- The `cgs/` directory contains vendored cgs sources with the exact-capture extension required by live resizing. Preserve its license and record source updates in `cgs/README.fork.md`.
- Run `bash scripts/verify-dot-port.sh` after changing the imported Rift features. Pass a configuration file as the optional argument to validate additional app rules.
- The verification script tests LaunchAgent serialization with an isolated HOME. It must not start or stop the user's service, change signing identities, or install the daemon.
- Desktop tests under `tests/rift-*-live-test.py` require a running matching daemon and macOS permissions. Native geometry tests alone do not prove composited content rendering; retain display video and daemon traces for live resize verification.
