# Rift upstream integration

The latest reviewed Rift revision is [b571fe7](https://github.com/acsandmann/rift/commit/b571fe76dfc4c0cc29ce22ff478165adda7c0a62), published on 2026-11-13. This integration covers all ten commits after the previous baseline, [6101a78](https://github.com/acsandmann/rift/commit/6101a78a4958e942853eb21cb67f8012fb293458).

## Included changes

| Upstream change | wm behavior |
| --- | --- |
| [73ad17c](https://github.com/acsandmann/rift/commit/73ad17cc459c40dc0939088494f71a950a17f08a) | Remove the superseded automatic app-actor retry and unresolved-PID termination reconciliation. Keep actor identity checks and queued rediscovery during shutdown. |
| [f3f4622](https://github.com/acsandmann/rift/commit/f3f46227c3fdf37523ee0c3a1637d15c47613f2b), [0384b45](https://github.com/acsandmann/rift/commit/0384b456732d129786019d5eae308e7a99220d0a), [9d484ca](https://github.com/acsandmann/rift/commit/9d484cadb6b53ddccf673f1e6250b1142ec5aba3) | Use Core Animation for overview camera transitions, pause idle display links, and defer capture resizing during camera motion. Prepare native geometry behind the overview and issue final native focus after closing the overview. |
| [0fc841e](https://github.com/acsandmann/rift/commit/0fc841ef92c90212b4924f73f2c3b8bcd8fb853d) | Preserve the existing wm test for geometry invalidation and add the upstream Space-destruction case. wm already preserves animations when only the snapshot revision changes. |
| [970c503](https://github.com/acsandmann/rift/commit/970c5035fd03b0a51e13c9c9cd326a3388c076cf) | Keep inactive workspace windows when AX identities are invalidated and an app temporarily reports an empty inventory. |
| [1c2c7ee](https://github.com/acsandmann/rift/commit/1c2c7eeed7c67073c678e3bcea2ec9888d22f04a), [9d484ca](https://github.com/acsandmann/rift/commit/9d484cadb6b53ddccf673f1e6250b1142ec5aba3) | Dispatch a newer final focus without waiting for the previous timeout. Allow one bounded grace period for slow final focus while preserving stacking-only requests. |
| [4ae023e](https://github.com/acsandmann/rift/commit/4ae023eaa28188ca089dc0ea97c896181656742c) | Exclude internal WindowServer Spaces from desktop topology and ignore destruction of untracked transient Spaces. Keep queued scrolling geometry authorized during app switching. |
| [b571fe7](https://github.com/acsandmann/rift/commit/b571fe76dfc4c0cc29ce22ff478165adda7c0a62) | Recover focus after authoritative removal even when an earlier AX invalidation cleared current focus. Preserve a native tab successor. |

The remaining commit, [3fff949](https://github.com/acsandmann/rift/commit/3fff9496423666b874326caeb7f4108858dde364), only bumps Rift to 0.8.2. wm already has that version. The upstream README branding removal is omitted because wm maintains its own README.

## Fork adaptations

wm retains its scrolling-only layout API, native window motion, menu bar, command names, configuration, release process, and vendored cgs sources. Overview regression fixtures configure scrolling geometry before creating windows because wm has no layout-switch command. Unit tests use the selected overview window without querying the live WindowServer for native focus.

The historical baseline in `scripts/check-fork-notices.py` and the archived import comparison remain fixed. They identify the original import and continue to check attribution across later source changes.

## Verification

Run the complete integration check on macOS with a desktop session:

```sh
bash scripts/verify-dot-port.sh
cargo test --locked -p wm-client
```

The integration run passed all 822 library tests, cgs exact-capture tests, identity and configuration checks, the native menu-bar example, both binary builds, CLI namespace checks, and the isolated LaunchAgent fixture. The fixture does not load a service.

Before the fixes, regression tests reproduced rapid-focus cancellation, slow-focus cancellation, and missing fallback focus after AX-first window removal. The same cases pass after integration.

Desktop overview rendering and native focus timing remain unverified. The integration does not install binaries or replace the running daemon. A matching daemon is required to check overview entry, pan, zoom, drag, repeated selection, and cross-display exit on the desktop. Do not treat passing geometry and unit tests as evidence of rendered animation quality.
