# Rift changes imported from dot

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
| Motion | Bounded native geometry writes and composited content during single and simultaneous window resizes. |
| Distribution | The `rift-patched` native identity and LaunchAgent XML escaping remain compatible with the dot installation. |

Live resizing uses the exact-capture extension in the vendored [cgs sources](../cgs/README.fork.md). The root Cargo dependency uses the relative `cgs/` path. dispatchr remains fixed by `Cargo.lock` at `5b4054c472d1108ab6ac690db6ae6eb5b0f94e62`.

Personal app rules, keyboard shortcuts, signing setup, installation wrappers, and the Swift close-window helper remain in dot. The native binary builds as `target/debug/rift` and retains the public CLI name `rift`. Service installation resolves a native `rift` directly. The dot installer names the installed executable `rift-patched`; its existing resolution and native bundle identity remain supported.

## Build and verify

Use an Apple Silicon Mac with Apple's Command Line Tools and Rust 1.95.0 or later. Run:

```sh
bash scripts/verify-dot-port.sh
```

To validate an additional configuration, pass its path:

```sh
bash scripts/verify-dot-port.sh /path/to/config.toml
```

The script runs the verifier regressions, cgs exact-capture tests, all Rift library tests on one thread, configuration validation, the native menu-bar example, and both executable builds. The service fixture writes a LaunchAgent into a temporary HOME and parses the generated XML. It does not load a LaunchAgent or change the running service.

The menu-bar example uses AppKit and needs a desktop session. A headless run does not establish desktop behavior.

To reproduce the imported source comparison from local checkouts with nightly rustfmt on PATH, run:

```sh
python3 scripts/check-dot-port.py /path/to/dot /path/to/acsandmann/cgs
```

The comparison uses the recorded source revisions, applies the ordered patches and the recorded upstream integration adjustments in a temporary directory, formats the reconstructed Rust files, and compares the result. Vendored cgs files retain their original formatting and are compared byte for byte. Pass `--rustfmt` to select the nightly formatter explicitly. Run this migration check before making further changes to the imported code.

## Verify on the desktop

The live checks require a matching running daemon, Accessibility access, and a disposable scrolling workspace. Composited resize checks also require Screen Recording access for the daemon. The scripts preserve the source checks' fixture and cleanup behavior. They default to `target/debug/rift-cli`; pass `--client` to use a different build or installation.

Run only the checks relevant to the change:

```sh
python3 tests/rift-settings-focus-live-test.py --app muse
python3 tests/rift-settings-focus-live-test.py --app anki
python3 tests/rift-window-lifecycle-live-test.py
python3 tests/rift-motion-live-test.py --output /private/tmp/rift-motion.json
python3 tests/rift-content-render-live-test.py --output /private/tmp/rift-content.json --video-dir /private/tmp/rift-content-video
python3 tests/rift-live-resize-live-test.py --run --video --trace-file /private/tmp/rift-live-resize.jsonl
python3 tests/rift-multi-resize-live-test.py --windows 2 --run --video --trace-file /private/tmp/rift-live-resize.jsonl
```

The Qt fixtures default to Anki's bundled Python and PyQt6. Pass `--qt-python` and `--qt-packages` for another runtime. Start the daemon with `RIFT_LIVE_RESIZE_TRACE` pointing to the same JSONL file before compositor checks. Inspect the display recording and traces for every participant. Smooth native bounds alone do not establish smooth rendered content.

For menu-bar changes, check native icon order, offscreen click-to-focus, workspace switching, overflow, and the original Rift menu. For Screen Zoom, cross a visible window boundary in both directions with Control released and stop immediately after crossing.
