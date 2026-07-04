//! `git-ents-server` — the standalone binary; see [`git_ents_server`] for the
//! `pub` `Args`/`Command` also embedded as `git ents server`.

use std::process::ExitCode;

use facet::Facet;
use figue::FigueBuiltins;

#[derive(Facet)]
struct Cli {
    #[facet(flatten)]
    args: git_ents_server::Args,
    #[facet(flatten)]
    builtins: FigueBuiltins,
}

fn main() -> ExitCode {
    let config = match figue::builder::<Cli>() {
        Ok(builder) => builder,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    }
    .cli(|cli| cli.args(std::env::args().skip(1)))
    .help(|help| {
        help.program_name("git-ents-server")
            .version(env!("CARGO_PKG_VERSION"))
    })
    .build();
    let cli: Cli = match figue::Driver::new(config).run().into_result() {
        Ok(output) => output.get(),
        Err(figue::DriverError::Help {
            text,
            suggestion: suggestion @ Some(_),
        }) => {
            println!("{text}");
            if let Some(s) = suggestion {
                println!("{}", s.render_pretty());
            }
            return ExitCode::FAILURE;
        }
        Err(error) => figue::DriverOutcome::<Cli>::err(error).unwrap(),
    };
    git_ents_server::run(cli.args)
}
