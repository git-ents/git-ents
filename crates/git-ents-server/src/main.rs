//! Git Ents server — helpful guardians of your git trees.

mod asciidoc;
mod checks;
mod http;
mod verify;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use clap::{CommandFactory, Parser, Subcommand};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(
    name = "git-ents-server",
    about = "Helpful guardians of your git trees."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Generate man pages into the given directory.
    #[arg(long, value_name = "DIR")]
    generate_man: Option<PathBuf>,

    /// Port to listen on.
    #[arg(long, env = "PORT", default_value = "8080")]
    port: u16,

    /// Directory holding the bare repositories served over HTTP.
    #[arg(long, env = "GIT_PROJECT_ROOT", default_value = "/data/repos")]
    data_dir: PathBuf,

    /// Secret seed for signed-push nonces. Setting it requires pushes to carry
    /// a signed-push certificate, enabling authentication against the signers.
    #[arg(long, env = "CERT_NONCE_SEED")]
    cert_nonce_seed: Option<String>,

    /// Directory of git hooks (a `pre-receive`) applied to every served repo.
    #[arg(long, env = "GIT_ENTS_HOOKS_DIR")]
    hooks_dir: Option<PathBuf>,

    /// Directory where the `post-receive` hook queues pushes for the check
    /// worker to run asynchronously.
    #[arg(
        long,
        env = "GIT_ENTS_CHECKS_QUEUE",
        default_value = "/data/checks-queue"
    )]
    checks_queue: PathBuf,
}

/// Subcommands that run instead of serving HTTP.
#[derive(Subcommand)]
enum Command {
    /// Verify a signed push against the authorized signers (a git `pre-receive`
    /// hook).
    PreReceive,
    /// Run the configured checks against a push in a Sprite (a git
    /// `post-receive` hook).
    PostReceive,
}

/// Shared handler state: where the bare repositories live, plus a lock that
/// serializes repository creation so concurrent first pushes cannot race.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) data_dir: PathBuf,
    pub(crate) init_lock: Arc<Mutex<()>>,
    /// When set, injected as `receive.certNonceSeed` so the backend demands a
    /// signed-push certificate the `pre-receive` hook can verify.
    pub(crate) cert_nonce_seed: Option<String>,
    /// When set, injected as `core.hooksPath` so every served repo runs the
    /// bundled `pre-receive` verifier.
    pub(crate) hooks_dir: Option<PathBuf>,
    /// Directory the `post-receive` hook queues pushes into and the check
    /// worker drains; passed down to the hook via [`checks::QUEUE_ENV`].
    pub(crate) checks_queue: PathBuf,
    /// In-memory web sessions: a browser's signed-in web key, held for the life
    /// of the process and never persisted.
    pub(crate) sessions: web::Sessions,
}

fn main() -> ExitCode {
    let args = Args::parse();

    if let Some(Command::PreReceive) = args.command {
        return match verify::pre_receive() {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("error: {reason}");
                ExitCode::FAILURE
            }
        };
    }

    if let Some(Command::PostReceive) = args.command {
        // A post-receive failure cannot undo the push; report and exit clean so
        // a runner hiccup never looks like a rejected push.
        if let Err(reason) = checks::post_receive() {
            eprintln!("checks: {reason}");
        }
        return ExitCode::SUCCESS;
    }

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
        init_lock: Arc::new(Mutex::new(())),
        cert_nonce_seed: args.cert_nonce_seed,
        hooks_dir: args.hooks_dir,
        checks_queue: args.checks_queue,
        sessions: web::new_sessions(),
    };

    // Drain queued pushes and run their checks for the life of the server.
    tokio::spawn(checks::worker(state.checks_queue.clone()));

    // The git smart-HTTP protocol streams whole packfiles through the request
    // body, so the default 2 MiB cap would reject any non-trivial push.
    let app = Router::new()
        .route("/healthz", get(http::health))
        .route("/", get(http::get_request))
        .route("/{*path}", get(http::get_request).post(http::post_request))
        .layer(DefaultBodyLimit::disable())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("error: failed to bind to port {}: {e}", args.port);
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
