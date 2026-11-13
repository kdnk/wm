# wm

wm is a tiling window manager for macOS, forked from [Rift](https://github.com/acsandmann/rift). System Integrity Protection can stay enabled.

## Build

Use macOS with Apple's Command Line Tools and Rust 1.95.0 or later.

```sh
cargo build --locked --release --bin wm --bin wm-cli
```

Put `target/release/wm` and `target/release/wm-cli` in a stable location on PATH.

## Configure and run

```sh
mkdir -p ~/.config/wm
cp wm.default.toml ~/.config/wm/config.toml
```

Stop other window managers and grant wm Accessibility access.

```sh
wm service install
wm service start
wm-cli query windows
```

## Verify changes

```sh
bash scripts/verify-dot-port.sh
```

See the [import and verification notes](docs/dot-port.md) and [configuration reference](https://acsandmann.github.io/rift-docs/reference/configuration/).

## License

The original source is licensed under [Apache 2.0](LICENSE). The vendored [cgs library](cgs/README.fork.md) is GPLv3. Distribution of the linked executable must comply with GPLv3, including corresponding-source requirements.
