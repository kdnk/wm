# wm

wm is a tiling window manager for macOS, forked from [Rift](https://github.com/acsandmann/rift). This fork integrates the window-management changes previously maintained in [kdnk/dot](https://github.com/kdnk/dot/tree/f502e0bdaac397f7ede56e2b4b384c018d8a4126/tools/rift).

wm supports scrolling columns, binary space partitioning, master-stack, floating, and stacked layouts. It includes focus follows mouse, native window lists in the menu bar, saved layouts, trackpad gestures, and animated window resizing. It does not require disabling System Integrity Protection.

## Build

Use macOS with Apple's Command Line Tools and Rust 1.95.0 or later.

```sh
cargo build --locked --release --bin wm --bin wm-cli
```

The executables are `target/release/wm` and `target/release/wm-cli`. Put both executables in a stable location on PATH before registering the service.

## Configure and run

Create a wm configuration from the included example:

```sh
mkdir -p ~/.config/wm
cp wm.default.toml ~/.config/wm/config.toml
```

Stop any other window manager before starting wm. Grant Accessibility access to the wm executable. Composited live resizing also needs Screen Recording access for the same executable.

```sh
wm service install
wm service start
wm-cli query windows
```

The service is registered as `git.kdnk.wm`. Configuration defaults to `~/.config/wm/config.toml`, and the saved layout defaults to `~/.wm/layout.ron`. Environment variables use the `WM_` prefix, including `WM_BS_NAME` and `WM_LIVE_RESIZE_TRACE`.

For an existing Rift configuration, copy the TOML file to the wm configuration path and update commands such as `rift-cli` to `wm-cli`. wm uses its own service, IPC identity, and state directory. Existing Rift services and settings remain separate.

## Verify changes

```sh
bash scripts/verify-dot-port.sh
```

The [import and verification notes](docs/dot-port.md) describe the native tests, desktop checks, and retained upstream provenance. The [Rift configuration reference](https://acsandmann.github.io/rift-docs/reference/configuration/) documents the inherited configuration format; use wm command names and paths with this fork.

## Origin and licenses

wm is an independent fork of Rift and is not an official Rift release. Rift began as a fork of [glide-wm](https://github.com/glide-wm/glide) and uses private APIs reverse engineered by yabai and other projects.

The original copyright and Apache 2.0 license text remain in [LICENSE](LICENSE). Modified source files identify the fork's changes. The vendored [cgs library](cgs/README.fork.md) is GPLv3; distribution of the linked executable must also comply with GPLv3, including its corresponding-source requirements. Dependency license notices are included in release archives.

The retained [manifesto](manifesto.md) and [roadmap](roadmap.md) describe the upstream Rift project. They are preserved as source history.
