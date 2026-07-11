//! `git ents`: parse, hand to [`git_ents::exe`], map error to exit code.

use std::process::ExitCode;

use git_ents::cli::Cli;

fn main() -> ExitCode {
    let cli: Cli = figue::from_std_args().unwrap();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match git_ents::exe::run(cli, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
