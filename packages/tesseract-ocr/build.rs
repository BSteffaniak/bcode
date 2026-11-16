use std::env;

/// Translates the linkage reported by `bcode_tesseract_sys` into cfgs, so the
/// wrapper only references directly linked Tesseract symbols when the sys
/// crate actually linked them. The link mode can be overridden through
/// `BCODE_TESSERACT_LINK_MODE`, so Cargo features alone cannot decide this.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(bcode_tesseract_linked)");
    println!("cargo::rustc-check-cfg=cfg(bcode_tesseract_bundled_static)");
    println!("cargo::rerun-if-env-changed=BCODE_TESSERACT_LINK_MODE");
    let linkage = env::var("DEP_TESSERACT_LINKAGE")
        .expect("bcode_tesseract_sys must report its Tesseract linkage");
    match linkage.as_str() {
        "system" | "vendored" => println!("cargo::rustc-cfg=bcode_tesseract_linked"),
        "bundled-static" => {
            println!("cargo::rustc-cfg=bcode_tesseract_linked");
            println!("cargo::rustc-cfg=bcode_tesseract_bundled_static");
        }
        "none" | "bundled-dynamic" => {}
        other => panic!("unsupported bcode_tesseract_sys linkage: {other}"),
    }
}
