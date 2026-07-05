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
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    // With zero CLI tokens (how Fly.io always runs this image — every setting
    // arrives via env vars, which `git_ents_server::run` reads itself),
    // `#[facet(flatten)]`'s all-`Option` `args` never gets a single populated
    // key, and figue reports the whole flattened group as a missing field
    // rather than an all-`None` value. Build it directly instead of parsing.
    if raw_args.is_empty() {
        return git_ents_server::run(git_ents_server::Args {
            command: None,
            port: None,
            data_dir: None,
            cert_nonce_seed: None,
            hooks_dir: None,
            web_signing_key: None,
            checks_queue: None,
        });
    }

    let config = match figue::builder::<Cli>() {
        Ok(builder) => builder,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    }
    .cli(|cli| cli.args(raw_args))
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
