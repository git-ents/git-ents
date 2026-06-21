//! Git Ents server — helpful guardians of your git trees.

mod http;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use clap::{CommandFactory, Parser};
use tokio::sync::Notify;

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

/// Shared handler state: where the bare repositories live.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) data_dir: PathBuf,
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

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(serve(args))
}

/// Bind the listener and serve until shutdown.
async fn serve(args: Args) -> ExitCode {
    let state = AppState {
        data_dir: args.data_dir,
    };

    // The git smart-HTTP protocol streams whole packfiles through the request
    // body, so the default 2 MiB cap would reject any non-trivial push.
    let mut app = Router::new()
        .route("/", get(http::health))
        .route("/healthz", get(http::health))
        .fallback(http::git)
        .layer(DefaultBodyLimit::disable())
        .with_state(state);

    // When `--max-requests` is set, count every response and signal shutdown
    // once the limit is reached so the process exits on its own.
    let shutdown = Arc::new(Notify::new());
    if let Some(max) = args.max_requests {
        let counter = Arc::new(AtomicUsize::new(0));
        let notify = shutdown.clone();
        app = app.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let counter = counter.clone();
                let notify = notify.clone();
                async move {
                    let response = next.run(req).await;
                    let handled = counter.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                    if handled >= max {
                        notify.notify_one();
                    }
                    response
                }
            },
        ));
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("error: failed to bind to port {}: {e}", args.port);
            return ExitCode::FAILURE;
        }
    };

    let server =
        axum::serve(listener, app).with_graceful_shutdown(async move { shutdown.notified().await });
    if let Err(e) = server.await {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
