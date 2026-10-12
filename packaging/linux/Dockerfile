# Build environment for the Linux x86_64 binaries (viewer + servers).
#
# Debian 12 (glibc 2.36) on purpose: binaries built here run on Debian 12 and
# 13, Ubuntu 22.04 and newer, and any other distribution with glibc >= 2.36.
# See packaging/linux/build.sh for how it is used.
FROM debian:bookworm

# Compiler, linker, and what vcpkg needs to build libvpx (nasm, make, perl,
# pkg-config, curl/zip/unzip/tar/git for vcpkg itself). Nothing for the GUI:
# winit, glutin and wgpu load X11/Wayland/GL/Vulkan with dlopen at run time.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential pkg-config nasm perl \
      ca-certificates curl git zip unzip tar xz-utils \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/opt/vcpkg:$PATH \
    VCPKG_ROOT=/opt/vcpkg \
    VCPKG_DISABLE_METRICS=1

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal \
           --default-toolchain stable --component rustfmt,clippy \
 && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"

RUN git clone https://github.com/microsoft/vcpkg /opt/vcpkg \
 && /opt/vcpkg/bootstrap-vcpkg.sh -disableMetrics
