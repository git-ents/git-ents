//! `git ents`: parse, hand to [`git_ents::exe`], map error to exit code.

use std::process::ExitCode;

use git_ents::cli::Cli;

fn main() -> ExitCode {
    let cli: Cli = figue::from_std_args().unwrap();
    // The `Stdout` handle, not a held `StdoutLock`: `git ents lsp`
    // (`lens.serve`) hands stdout to `lsp-server`, whose writer thread takes
    // the lock itself, so holding it here for the whole command would
    // deadlock that thread. Every other command writes through the handle's
    // own per-write lock, which is behaviorally identical for their
    // single-threaded output.
    let mut out = std::io::stdout();
    match git_ents::exe::run(cli, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
