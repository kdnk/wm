// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
fn main() {
    println!("cargo:rustc-link-search=framework=/System/Library/PrivateFrameworks");
    println!("cargo:rustc-link-lib=framework=SkyLight");
}
