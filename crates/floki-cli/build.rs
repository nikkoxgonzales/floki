//! Embeds version and author details in `flk.exe` (Properties → Details).

#[path = "../../build-support/version_rc.rs"]
mod version_rc;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../build-support/version_rc.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let rc = out.join("flk.rc");
    std::fs::write(
        &rc,
        version_rc::version_rc("flk", "Floki command-line search"),
    )
    .expect("write flk.rc");
    embed_resource::compile_for(&rc, ["flk"], embed_resource::NONE)
        .manifest_optional()
        .expect("compile the version resource");
}
