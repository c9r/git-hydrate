use std::process::ExitCode;

fn main() -> ExitCode {
    git_hydrate::cli::dehydrate_main(std::env::args_os())
}
