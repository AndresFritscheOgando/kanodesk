//! Checks that libvpx is where `env-libvpx-sys` will look for it.
//!
//! `.cargo/config.toml` points `VPX_LIB_DIR` at the Windows vcpkg tree for
//! every target (Cargo's `[env]` cannot depend on the target). Elsewhere the
//! real path must come from the environment; without it the link would pick
//! up whatever system libvpx exists, whose ABI may not match the 1.13
//! bindings. Fail early with the fix instead.

fn main() {
    println!("cargo:rerun-if-env-changed=VPX_LIB_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        return;
    }
    let dir = std::env::var_os("VPX_LIB_DIR").map(std::path::PathBuf::from);
    let found = dir.as_ref().is_some_and(|d| {
        ["libvpx.a", "libvpx.so"]
            .iter()
            .any(|f| d.join(f).is_file())
    });
    if !found {
        panic!(
            "libvpx not found in VPX_LIB_DIR ({}). On Linux, run `packaging/linux/build.sh env` \
             and export what it prints (vcpkg's x64-linux libvpx, linked statically); see the \
             README, \"Linux (viewer)\".",
            dir.map(|d| d.display().to_string())
                .unwrap_or_else(|| "not set".into())
        );
    }
}
