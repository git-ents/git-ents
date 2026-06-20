//! Git Ents server — helpful guardians of your git trees.

mod http;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};

#[derive(Parser)]
#[command(
    name = "git-ents-server",
    about = "Helpful guardians of your git trees."
)]
struct Args {
    /// Generate man pages into the given directory.
    #[arg(long, value_name = "DIR")]
    generate_man: Option<PathBuf>,

    /// Port to listen on.
    #[arg(long, env = "PORT", default_value = "8080")]
    port: u16,

    /// Directory holding the bare repositories served over HTTP.
    #[arg(long, env = "GIT_PROJECT_ROOT", default_value = "/data/repos")]
    data_dir: PathBuf,

    /// Stop after handling this many requests.
    #[arg(long)]
    max_requests: Option<usize>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    if let Some(dir) = args.generate_man {
        let cmd = Args::command();
        if let Err(e) = clap_mangen::generate_to(cmd, dir) {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let server = match tiny_http::Server::http(format!("0.0.0.0:{}", args.port)) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("error: failed to bind to port {}: {e}", args.port);
            return ExitCode::FAILURE;
        }
    };

    let mut count: usize = 0;
    for request in server.incoming_requests() {
        if is_health(&request) {
            let _health = request.respond(tiny_http::Response::from_string("ok"));
        } else if let Err(e) = http::handle(request, &args.data_dir) {
            eprintln!("error: {e}");
        }
        count = count.saturating_add(1);
        if args.max_requests.is_some_and(|max| count >= max) {
            break;
        }
    }

    ExitCode::SUCCESS
}

/// A liveness probe (and the `/` root) that does not touch git.
fn is_health(request: &tiny_http::Request) -> bool {
    matches!(request.url(), "/" | "/healthz")
}
