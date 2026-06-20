//! Git Ents server — helpful guardians of your git trees.

use std::io::{Read, Write};
use std::net::TcpListener;
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

    let listener = match TcpListener::bind(format!("0.0.0.0:{}", args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: failed to bind to port {}: {e}", args.port);
            return ExitCode::FAILURE;
        }
    };

    for (count, stream) in listener.incoming().enumerate() {
        if let Ok(mut stream) = stream {
            let mut buf = [0u8; 4096];
            let _read = stream.read(&mut buf);
            let _write = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
        if args
            .max_requests
            .is_some_and(|max| count.saturating_add(1) >= max)
        {
            break;
        }
    }

    ExitCode::SUCCESS
}
