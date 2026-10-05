//! On Windows, links the icon Explorer and the taskbar show. The .res is prebuilt from
//! assets/apitool.rc, so building needs no resource compiler.

fn main() {
    println!("cargo:rerun-if-changed=assets/apitool.res");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg-bins={dir}/assets/apitool.res");
    }
}
