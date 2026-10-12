#!/usr/bin/env bash
# Builds KanoDesk on Linux x86_64 (the viewer and the servers; hosting from
# Linux is not supported yet).
#
#   packaging/linux/build.sh env       print the libvpx variables to export
#   packaging/linux/build.sh check     fmt, clippy -D warnings, tests
#   packaging/linux/build.sh package   release build + dist/kanodesk-<version>-linux-x86_64.tar.gz
#
# Needs: a Rust toolchain, a C toolchain, pkg-config, nasm and vcpkg
# (VCPKG_ROOT or `vcpkg` on PATH). libvpx comes from vcpkg (vcpkg.json, the
# same pinned 1.13.1 as on Windows) and is linked statically, so the binaries
# do not depend on the distribution's libvpx.
#
# VCPKG_INSTALL_ROOT (default ./vcpkg_installed) is where vcpkg installs; set
# it elsewhere when the checkout is shared with a Windows build, because a
# manifest-mode install removes other triplets' packages from its root.
# packaging/linux/Dockerfile is a Debian 12 build image; see the README.
set -euo pipefail

cd "$(dirname "$0")/../.."
root=$PWD
triplet=x64-linux
install_root=${VCPKG_INSTALL_ROOT:-$root/vcpkg_installed}

vcpkg_bin() {
    if [ -n "${VCPKG_ROOT:-}" ] && [ -x "$VCPKG_ROOT/vcpkg" ]; then
        echo "$VCPKG_ROOT/vcpkg"
    elif command -v vcpkg >/dev/null; then
        command -v vcpkg
    else
        echo "vcpkg not found: set VCPKG_ROOT or put vcpkg on PATH" >&2
        exit 1
    fi
}

install_libvpx() {
    if [ ! -f "$install_root/$triplet/lib/libvpx.a" ]; then
        "$(vcpkg_bin)" install --triplet "$triplet" --x-install-root="$install_root" --disable-metrics
    fi
    export VPX_LIB_DIR="$install_root/$triplet/lib"
    export VPX_INCLUDE_DIR="$install_root/$triplet/include"
    export VPX_STATIC=1
}

case "${1:-}" in
env)
    install_libvpx >&2
    echo "export VPX_LIB_DIR='$VPX_LIB_DIR' VPX_INCLUDE_DIR='$VPX_INCLUDE_DIR' VPX_STATIC=1"
    ;;
check)
    install_libvpx
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    ;;
package)
    install_libvpx
    cargo build --release --locked -p kanodesk-desktop -p kanodesk-idserver -p kanodesk-relay
    target=${CARGO_TARGET_DIR:-$root/target}
    # tr: a Windows checkout may have CRLF line endings.
    version=$(tr -d '\r' < Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
    name=kanodesk-$version-linux-x86_64
    stage=$(mktemp -d)
    mkdir "$stage/$name"
    for bin in kanodesk kanodesk-idserver kanodesk-relay; do
        install -m 755 "$target/release/$bin" "$stage/$name/$bin"
        strip "$stage/$name/$bin"
    done
    install -m 644 packaging/linux/QUICKSTART.md LICENSE-MIT LICENSE-APACHE "$stage/$name/"
    mkdir -p dist
    tar -C "$stage" --owner=0 --group=0 -czf "dist/$name.tar.gz" "$name"
    rm -rf "$stage"
    (cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
    echo "dist/$name.tar.gz"
    ;;
*)
    sed -n '2,8p' "$0"
    exit 2
    ;;
esac
