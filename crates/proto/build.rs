//! Compiles `proto/kanodesk/v1/*.proto` with protox (a pure-Rust protobuf
//! compiler, so no `protoc` binary is needed) and generates prost types.

const FILES: &[&str] = &[
    "proto/kanodesk/v1/control.proto",
    "proto/kanodesk/v1/display.proto",
    "proto/kanodesk/v1/input.proto",
    "proto/kanodesk/v1/stats.proto",
    "proto/kanodesk/v1/video.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for file in FILES {
        println!("cargo:rerun-if-changed={file}");
    }
    let descriptors = protox::compile(FILES, ["proto"])?;
    prost_build::Config::new().compile_fds(descriptors)?;
    Ok(())
}
