# wm changes imported from dot

This fork includes the Rift changes previously built from patches in [kdnk/dot](https://github.com/kdnk/dot/tree/f502e0bdaac397f7ede56e2b4b384c018d8a4126/tools/rift). The changes are integrated into the source and require no patch step during a normal Cargo build.

The patches used Rift 0.8.1 at `b5cf5c7f7b9ffe7364377808df1ab1a4ecd24637`. This port retains the fork's newer upstream changes through `6101a78`. Upstream already preserves the current animation frame when replacing an animation. The port also preserves its velocity.

## Imported behavior

| Area | Change |
| --- | --- |
| Focus | Cursor warps follow external activation. Hover focus uses current WindowServer coordinates during Screen Zoom. |
| Menu bar | Native window lists, grouped application icons, workspace capsules, overflow, and measured placement around application menus and the notch. |
| Window lifecycle | Regression coverage for closed windows and duplicate space snapshots. Suspended windows retain their logical layout slots. |
| Settings windows | Explicit management rules can admit attached standard windows and positively identified nonmodal dialogs. |
| Configuration | The `config-check` example validates the complete configuration with the native parser. |
| Scrolling layout | Stacked-row resizing, selection of participating neighboring columns, and bounded weighted size allocation. |
| Motion | Continuous native position and size animation with bounded geometry writes and velocity preserved through target changes. |
| Distribution | LaunchAgent XML escaping preserves installation paths and environment values. The fork uses the `git.kdnk.wm` identity. |

The composited resize presentation imported from dot has been removed. Size changes use the native animation path from `macos-window-motion.patch`, including intermediate native sizes and velocity-preserving target changes. The exact-capture extension remains in the vendored [cgs sources](../cgs/README.fork.md) for source provenance. The root Cargo dependency uses the relative `cgs/` path. dispatchr remains fixed by `Cargo.lock` at `5b4054c472d1108ab6ac690db6ae6eb5b0f94e62`.

Personal app rules, keyboard shortcuts, signing setup, installation wrappers, and the Swift close-window helper remain in dot. The native binary builds as `target/debug/wm`, and the client builds as `target/debug/wm-cli`. Service installation registers the current native executable. Configuration, state, IPC, and environment variables use the wm namespace. Existing dot installation wrappers still target the older patched Rift distribution and require a separate migration before they can install wm.

## Build and verify

Use an Apple Silicon Mac with Apple's Command Line Tools and Rust 1.95.0 or later. Run:

```sh
bash scripts/verify-dot-port.sh
```

To validate an additional configuration, pass its path:

```sh
bash scripts/verify-dot-port.sh /path/to/config.toml
```

The script runs the verifier regressions, cgs exact-capture tests, all wm library tests on one thread, default-path identity checks, configuration validation, the native menu-bar example, and both executable builds. It checks the CLI names and an isolated IPC namespace. The service fixture writes a LaunchAgent into a temporary HOME and parses the generated XML while a shell wrapper is first on PATH. It does not load a LaunchAgent or change the running service.

The menu-bar example uses AppKit and needs a desktop session. A headless run does not establish desktop behavior.

To reproduce the original import comparison from local checkouts with nightly rustfmt on PATH, run:

```sh
python3 scripts/check-dot-port.py /path/to/dot /path/to/acsandmann/cgs
```

The comparison reconstructs the recorded source revisions and compares them with the archived import commit `61eb68b`, before the rename to wm. It applies the ordered patches and recorded upstream integration adjustments, formats the reconstructed Rust files, and compares the result. This archival check proves the import's provenance; current behavior is checked by `verify-dot-port.sh`. Pass `--rustfmt` to select the nightly formatter explicitly.

## Verify on the desktop

The live checks require a matching running daemon, Accessibility access, and a disposable scrolling workspace. The imported `tests/rift-*-test.py` filenames retain their historical names, while their commands and environment variables use wm. They default to `target/debug/wm-cli`; pass `--client` to use a different build or installation.

Run only the checks relevant to the change:

```sh
python3 tests/rift-settings-focus-live-test.py --app muse
python3 tests/rift-settings-focus-live-test.py --app anki
python3 tests/rift-window-lifecycle-live-test.py
python3 tests/rift-motion-live-test.py --output /private/tmp/rift-motion.json
python3 tests/rift-content-render-live-test.py --output /private/tmp/rift-content.json --video-dir /private/tmp/rift-content-video
```

The Qt fixtures default to Anki's bundled Python and PyQt6. Pass `--qt-python` and `--qt-packages` for another runtime. Inspect the retained content video as well as native geometry. Smooth native bounds alone do not establish smooth rendered content.

The retained `rift-live-resize-live-test.py` and `rift-multi-resize-live-test.py` scripts describe the historical compositor implementation. They require compositor traces and do not validate the current native animation path. `WM_LIVE_RESIZE_TRACE` is no longer emitted by wm.

For menu-bar changes, check native icon order, offscreen click-to-focus, workspace switching, overflow, and the wm controls menu. For Screen Zoom, cross a visible window boundary in both directions with Control released and stop immediately after crossing.
