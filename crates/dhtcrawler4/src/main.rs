//! The `dhtcrawler4` binary. Everything lives in the library so that the
//! end-to-end tests can drive the same code.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    dhtcrawler4::cli::main()
}
