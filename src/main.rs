//! The `moi` binary. All logic lives in the `cestmoi` library.

fn main() -> std::process::ExitCode {
    cestmoi::cli::run()
}
