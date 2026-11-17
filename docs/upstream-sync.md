# Rift upstream integration

The latest reviewed Rift revision is [ecdee64](https://github.com/acsandmann/rift/commit/ecdee64e52855b4fb8d13f4441b108cefd866c88), published on 2026-11-17. The November 17 integration reviews all eleven commits after [b571fe7](https://github.com/acsandmann/rift/commit/b571fe76dfc4c0cc29ce22ff478165adda7c0a62).

## November 17 integration

| Upstream change | wm disposition |
| --- | --- |
| [ba80b9d](https://github.com/acsandmann/rift/commit/ba80b9d99a8555d61fcdd9b0ecf83f38b0019672) | Suppress Enhanced UI only when native frame writes resize a window. Retain wm's scrolling bounds and native animation path. |
| [b273aed](https://github.com/acsandmann/rift/commit/b273aed9093ecaa3c7580a62ff3db713f52e7eff) | Match app rules against a raw WindowServer tag mask. Every bit in the configured nonzero mask must be present. |
| [cda9a6e](https://github.com/acsandmann/rift/commit/cda9a6e19ad7be9d963879a6e6e889313f1f3067) | Retain wm's existing test-only mouse and separate-Spaces implementations. Also register directly inserted fixture windows as synthetic so native Space queries cannot resolve fake IDs against the live desktop. |
| [8a012e8](https://github.com/acsandmann/rift/commit/8a012e8863d7e84b8c2e7d7c84ae58478494953b) | Omit the settings-popover fixture change because wm removed the settings editor. |
| [9a8408b](https://github.com/acsandmann/rift/commit/9a8408b86615c227d272d6b8738a0e96fa4984eb) | Import the config validation message that describes all matching conditions. Omit settings-editor changes. |
| [92e086d](https://github.com/acsandmann/rift/commit/92e086d3ac658ceaec3b7352c64b694706d672ea) | Retain wm's existing executable installation, signing identity, LaunchAgent, and release workflow. App-bundle packaging would change the fork's distribution contract. |
| [23d52b5](https://github.com/acsandmann/rift/commit/23d52b5a36df2597e2880ca243061445d989f5b4) | Omit the Rift 0.8.3 version bump. wm versions independently. |
| [b2f9325](https://github.com/acsandmann/rift/commit/b2f93252e0b07d27bd865c8851ee09b05050e946) | Seed empty visible displays in global workspace mode and leave a replacement when explicitly moving the last workspace away. Preserve ownership when displays reconnect or reorder. |
| [8d93771](https://github.com/acsandmann/rift/commit/8d937711a934b963d13505c65813993ade93e9f5) | Reuse warm overview captures, retarget camera motion, and verify native geometry and focus before revealing the desktop. Recover failed handoffs to an interactive overview. |
| [fbf4bb6](https://github.com/acsandmann/rift/commit/fbf4bb6bef222db7507431dbb821f747b5bbc683) | Exclude the source display when a directional display command wraps across overlapping display edges. |
| [ecdee64](https://github.com/acsandmann/rift/commit/ecdee64e52855b4fb8d13f4441b108cefd866c88) | Already included in wm's `ca1415c`. Ignore nonactivating attached surfaces as hover-focus blockers while retaining modal protection. wm also has a regression test for the observed Zoom tags. |

The [integration decision log](upstream-sync-decisions.tsv) records verification and fork choices.

## November 13 integration

The previous integration covered all ten commits after [6101a78](https://github.com/acsandmann/rift/commit/6101a78a4958e942853eb21cb67f8012fb293458), through [b571fe7](https://github.com/acsandmann/rift/commit/b571fe76dfc4c0cc29ce22ff478165adda7c0a62).

### Included changes

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

## Verification commands

Run the complete integration check on macOS with a desktop session:

```sh
bash scripts/verify-dot-port.sh
cargo test --locked -p wm-client
```

The final November 17 run passed the complete `verify-dot-port.sh` check, including all 912 library tests, three cgs exact-capture tests, the identity test, 14 client tests, and 11 protocol tests. Configuration validation, native menu-bar and drag-preview fixtures, the binary build, CLI namespace checks, and the isolated LaunchAgent fixture also passed. Native fixtures emitted their expected PASS summaries. Nightly `cargo fmt --all --check` passed.

The initial sandbox run could not receive filesystem notifications in two watcher tests. An unrestricted baseline passed those tests but exposed three existing display-fixture failures. A new regression reproduced direct fixture IDs querying real native Spaces. Registering those IDs as synthetic fixed the failures, and the final unrestricted integration run passed without skipped tests.

Independent review found an upstream assumption that needed a fork adaptation. Upstream's bounded interactive frames do not resize windows, but wm retains native bounded resizing. The initial port skipped Enhanced UI suppression for that path. The repair acquires suppression through the frame writer's resize callback. Its regression reproduced the missing suppression before the repair and checks bounded resizing, position-only motion, unchanged frames, and stale size hints after the repair.

The November 13 integration run passed all 822 library tests, cgs exact-capture tests, identity and configuration checks, the native menu-bar example, both binary builds, CLI namespace checks, and the isolated LaunchAgent fixture. The fixture does not load a service.

Before the November 13 fixes, regression tests reproduced rapid-focus cancellation, slow-focus cancellation, and missing fallback focus after AX-first window removal. Those cases passed after integration.

Desktop overview rendering and native focus timing remain unverified. The integration does not install binaries or replace the running daemon. A matching daemon is required to check overview entry, pan, zoom, drag, repeated selection, and cross-display exit on the desktop. Do not treat passing geometry and unit tests as evidence of rendered animation quality.
