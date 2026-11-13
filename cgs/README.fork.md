# Vendored cgs

This directory contains cgs 0.5.2 from [acsandmann/cgs](https://github.com/acsandmann/cgs/tree/bd7cdbc171904e7d6057cac074afbd72ce5c1bf9), revision `bd7cdbc171904e7d6057cac074afbd72ce5c1bf9`.

The source includes the exact capture changes from `tools/rift/cgs-exact-capture.patch` in [kdnk/dot](https://github.com/kdnk/dot). wm uses `CaptureResolution::Exact` and `Frame::native_pixel_size()` to wait for a resized window's native content. The three modified capture files and adjusted manifest carry dated fork notices. The library retains its cgs name and original license.

Only the crate manifest, build script, library sources, and license files are included. Manifest entries for omitted upstream native tests and examples are removed. The crate remains excluded from the Rift workspace and is used through a relative path dependency.

The upstream GPL-3.0 license is in `LICENSE`. The third-party notice is in `third-party/ProgressiveBlurHeader.LICENSE`.
