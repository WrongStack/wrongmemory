//! `wrongmemoryd`: the persistent per-user WrongMemory service.
//!
//! Startup order (architecture §1, §9): private directories -> OS-backed
//! singleton (first named-pipe instance) -> registry open/migrate ->
//! credential and discovery publication -> accept authenticated clients.
//! Client EOF never stops the daemon; only an authenticated host
//! `daemon.shutdown` or a console signal does (WM-INV-12).

mod log;
mod projects;
mod server;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use wrongmemory_ipc::DataRoot;

#[derive(Debug, Parser)]
#[command(
    name = "wrongmemoryd",
    version,
    about = "WrongMemory per-user memory service"
)]
struct Args {
    /// Data root (default: WRONGMEMORY_HOME or ~/.wrongmemory).
    #[arg(long)]
    data_root: Option<PathBuf>,
    /// Maximum time to drain in-flight mutations on graceful shutdown.
    #[arg(long, default_value_t = 15)]
    drain_seconds: u64,
}

/// Exit code when another daemon already owns this data root's endpoint.
pub const EXIT_ALREADY_RUNNING: u8 = 3;

fn main() -> ExitCode {
    let args = Args::parse();
    let root = match DataRoot::resolve(args.data_root.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wrongmemoryd: cannot resolve data root: {e}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(16)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wrongmemoryd: cannot start runtime: {e}");
            return ExitCode::from(70);
        }
    };
    let config = server::Config {
        root,
        drain: Duration::from_secs(args.drain_seconds),
    };
    match runtime.block_on(server::run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(server::StartError::AlreadyRunning(endpoint)) => {
            eprintln!("wrongmemoryd: another daemon already owns {endpoint}");
            ExitCode::from(EXIT_ALREADY_RUNNING)
        }
        Err(server::StartError::Other(e)) => {
            eprintln!("wrongmemoryd: {e:#}");
            ExitCode::from(70)
        }
    }
}
