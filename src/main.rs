use std::process::ExitCode;

fn main() -> ExitCode {
    git_hydrate::cli::main(std::env::args_os())
}
