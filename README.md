# wm

wm is a tiling window manager for macOS, forked from [Rift](https://github.com/acsandmann/rift). System Integrity Protection can stay enabled.

## Install a release

On macOS with Python 3.8 or later and curl, run:

```sh
curl -fsSL https://raw.githubusercontent.com/kdnk/wm/main/install.sh | sh
```

The installer downloads the latest universal macOS release and verifies the executable against the pinned signing certificate. A repository checkout, GitHub CLI, and Rust toolchain are not required. It installs `wm` in `~/.local/bin`. Add that directory to PATH.

`wm` now includes the former `wm-cli` commands. Update scripts and shortcuts to use `wm query`, `wm execute`, and `wm subscribe`. Existing `wm-cli` installations are left untouched. Older two-binary releases require a compatible older installer; see the [release installation notes](docs/release-signing.md#install-a-signed-release).

Pass `--version TAG` to select a single-binary release, or `--prefix PATH` to change the installation prefix:

```sh
curl -fsSL https://raw.githubusercontent.com/kdnk/wm/main/install.sh | sh -s -- --prefix "$HOME/.local"
```

From a checkout, `python3 scripts/install-release.py` accepts the same options and a local `.tar.gz` archive for offline installation. The installer preserves the release signatures and your configuration. Service control remains explicit.

To update a running installation, stop wm, run the installer, then start wm again:

```sh
wm service stop
curl -fsSL https://raw.githubusercontent.com/kdnk/wm/main/install.sh | sh
wm service start
wm query windows
```

## Build

Use macOS with Apple's Command Line Tools and Rust 1.95.0 or later.

```sh
cargo build --locked --release --bin wm
```

Put `target/release/wm` in a stable location on PATH.

## Configure and run

```sh
mkdir -p ~/.config/wm
test -e ~/.config/wm/config.toml || cp ~/.local/share/wm/release/wm.default.toml ~/.config/wm/config.toml
```

The release installs its default configuration and licenses in `~/.local/share/wm/release`. Adjust the source path if you used a different prefix. When building from source, use `wm.default.toml` from the checkout.

Edit `~/.config/wm/config.toml` to configure wm. Changes reload automatically while
`[settings].hot_reload` is enabled, which is the default. To reload manually, run
`wm execute config reload`.

wm uses niri-style scrolling columns. Columns can contain multiple windows. Other
layout modes, anchored navigation, and floating controls are removed. When updating
an older configuration, remove their settings and keybindings. Redundant
`floating = false` app-rule settings are ignored. Use `manage = false` to leave a
window unmanaged.

Stop other window managers and grant wm Accessibility access.

```sh
wm service install
wm service start
wm query windows
```

## Close a window

```sh
wm close-window
```

The command presses the focused window's native close button, including all its tabs. With the wm daemon running, it selects the next tiled neighbor after the window disappears, or the previous neighbor at the end. It also works without the daemon. `wm execute window close` retains Command-W behavior for closing a tab.

## Subscribe to events

```sh
wm subscribe mach windows_changed
```

The command prints matching events until the process exits. Press Ctrl-C to end the subscription. The former `wm subscribe unsub-mach` command is removed because a separate process cannot unsubscribe the original subscriber.

## Verify changes

```sh
bash scripts/verify-dot-port.sh
```

See the [import and verification notes](docs/dot-port.md) and [configuration reference](https://acsandmann.github.io/rift-docs/reference/configuration/).

## License

The original source is licensed under [Apache 2.0](LICENSE). The vendored [cgs library](cgs/README.fork.md) is GPLv3. Distribution of the linked executable must comply with GPLv3, including corresponding-source requirements.
