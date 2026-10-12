//! `kanodesk` binary entry point. The logic lives in the `kanodesk_desktop` library.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(kanodesk_desktop::main_with_args(&args));
}
