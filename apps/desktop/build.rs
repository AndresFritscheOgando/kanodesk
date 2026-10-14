//! Gives `kanodesk.exe` its icon on Windows; nothing to do elsewhere.

fn main() {
    println!("cargo:rerun-if-changed=assets/kanodesk.rc");
    println!("cargo:rerun-if-changed=assets/kanodesk.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/kanodesk.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("compiling the Windows resources");
    }
}
