//! Git Ents server — helpful guardians of your git trees.
//!
//! [`Args`] is `pub` so `git ents` can embed this server as its own `server`
//! subcommand, alongside the standalone `git-ents-server` binary.

mod asciidoc;
mod http;
mod markdown;
/// MIME-keyed document rendering (HTML and plain-text), shared by the web
/// UI and the `git-ents` CLI, which embeds this crate as a library.
pub mod render;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use facet::Facet;
use figue::{self as args};
use tokio::sync::Mutex;

/// Command-line arguments, layered over the matching environment variables
/// (the flag always wins) since that is how this server is configured on
/// Fly.io.
#[derive(Facet, Debug)]
pub struct Args {
    /// Subcommand that runs instead of serving HTTP.
    #[facet(args::subcommand)]
    pub command: Option<Command>,

    /// Port to listen on ($PORT, default 8080).
    #[facet(args::named)]
    pub port: Option<u16>,

    /// Directory holding the bare repositories served over HTTP
    /// ($GIT_PROJECT_ROOT, default `/data/repos`).
    #[facet(args::named)]
    pub data_dir: Option<PathBuf>,

    /// Secret seed for signed-push nonces ($CERT_NONCE_SEED). Setting it
    /// requires pushes to carry a signed-push certificate, enabling
    /// authentication against the signers.
    #[facet(args::named)]
    pub cert_nonce_seed: Option<String>,

    /// Directory of git hooks (a `pre-receive`) applied to every served repo
    /// ($GIT_ENTS_HOOKS_DIR).
    #[facet(args::named)]
    pub hooks_dir: Option<PathBuf>,

    /// The server's own SSH private key, used to sign browser-made edits
    /// ($GIT_ENTS_WEB_SIGNING_KEY). Its public half must be a member of any
    /// repo edited through the web. Editing is disabled unless this is set.
    #[facet(args::named)]
    pub web_signing_key: Option<PathBuf>,

    /// Directory where the `post-receive` hook queues pushes for the check
    /// worker to run asynchronously ($GIT_ENTS_CHECKS_QUEUE, default
    /// `/data/checks-queue`).
    #[facet(args::named)]
    pub checks_queue: Option<PathBuf>,
}

// @relation(server.embeddable)
/// Subcommands that run instead of serving HTTP.
#[derive(Facet, Debug)]
#[repr(u8)]
pub enum Command {
    /// Verify a signed push against the authorized signers (a git
    /// `pre-receive` hook).
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
    /// worker drains; passed down to the hook via [`git_effect::engine::QUEUE_ENV`].
    pub(crate) checks_queue: PathBuf,
    /// In-memory web sessions: a browser's signed-in public key, held for the
    /// life of the process and never persisted.
    pub(crate) sessions: web::Sessions,
    /// Outstanding one-time sign-in challenges awaiting a signature.
    pub(crate) challenges: web::Challenges,
    /// The server's own signing key for browser-made edits; `None` disables
    /// editing.
    pub(crate) web_signing_key: Option<PathBuf>,
    /// Live output for checks the worker currently has running, polled by the
    /// Checks tab's live view.
    pub(crate) live_runs: git_effect::engine::LiveRegistry,
}

/// The non-empty value of the environment variable `key`, or `None`.
fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

// @relation(server.embeddable, deploy.fly)
/// Run the server: dispatch `pre-receive`/`post-receive`, or serve HTTP.
/// `args`' flags win over their matching environment variable, which in turn
/// wins over the hardcoded default.
pub fn run(args: Args) -> ExitCode {
    if let Some(Command::PreReceive) = args.command {
        return match git_signed_push::pre_receive() {
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
        if let Err(reason) = git_effect::engine::post_receive() {
            eprintln!("effects: {reason}");
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
    let port = args
        .port
        .or_else(|| env_var("PORT").and_then(|value| value.parse().ok()))
        .unwrap_or(8080);
    let data_dir = args
        .data_dir
        .or_else(|| env_var("GIT_PROJECT_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/data/repos"));
    let checks_queue = args
        .checks_queue
        .or_else(|| env_var("GIT_ENTS_CHECKS_QUEUE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/data/checks-queue"));
    let cert_nonce_seed = args.cert_nonce_seed.or_else(|| env_var("CERT_NONCE_SEED"));
    let hooks_dir = args
        .hooks_dir
        .or_else(|| env_var("GIT_ENTS_HOOKS_DIR").map(PathBuf::from));
    let web_signing_key = args
        .web_signing_key
        .or_else(|| env_var("GIT_ENTS_WEB_SIGNING_KEY").map(PathBuf::from));

    let state = AppState {
        data_dir,
        init_lock: Arc::new(Mutex::new(())),
        cert_nonce_seed,
        hooks_dir,
        checks_queue,
        sessions: web::new_sessions(),
        challenges: web::new_challenges(),
        web_signing_key,
        live_runs: git_effect::engine::new_live_registry(),
    };

    // Drain queued pushes and run their effects for the life of the server:
    // the Sprite backend when `SPRITES_TOKEN` says this is the hosted
    // deployment, the local Docker backend otherwise — see
    // `git_effect::engine::default_backend`.
    tokio::spawn(git_effect::engine::worker(
        state.checks_queue.clone(),
        state.live_runs.clone(),
        git_effect::engine::default_backend(),
    ));

    // @relation(protocol.routing, deploy.health)
    // The git smart-HTTP protocol streams whole packfiles through the request
    // body, so the default 2 MiB cap would reject any non-trivial push.
    let app = Router::new()
        .route("/healthz", get(http::health))
        .route("/", get(http::get_request))
        // @relation(checks.debug)
        .route("/_debug/{*path}", get(web::handshake))
        .route("/{*path}", get(http::get_request).post(http::post_request))
        .layer(DefaultBodyLimit::disable())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("error: failed to bind to port {port}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
