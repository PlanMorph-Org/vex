//! `vex-serve` — server-side daemon for vex remote operations.
//!
//! Designed to be invoked from `sshd` via `ForceCommand`:
//!
//! ```text
//! command="/usr/local/bin/vex-serve --repo-root /var/lib/planmorph-vex \
//!          --user <user-id>"  ssh-ed25519 AAAA... user@host
//! ```
//!
//! The client side (`vex push`/`vex fetch`/`vex clone`) opens an SSH
//! connection, the forced command spawns `vex-serve` against the user's
//! stdin/stdout, and the protocol runs over that pipe.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;

use vex_serve::serve_session;

#[derive(Parser, Debug)]
#[command(name = "vex-serve", version, about = "Vex remote daemon.")]
struct Args {
    /// Filesystem directory containing all hosted repositories. Required.
    #[arg(long, value_name = "DIR")]
    repo_root: PathBuf,

    /// Authenticated user identifier (recorded in audit logs). Optional —
    /// defaults to `$USER` for local testing.
    #[arg(long, value_name = "ID")]
    user: Option<String>,

    /// Refuse all push attempts. Sets `ServeConfig::allow_push = false`.
    #[arg(long)]
    read_only: bool,
}

fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();

    let args = Args::parse();
    if let Err(err) = run(args) {
        eprintln!("vex-serve error: {err:#}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run(args: Args) -> anyhow::Result<()> {
    let architur = vex_serve::architur::ArchiturClient::from_env();
    let config = vex_serve::ServeConfig {
        repo_root: args.repo_root,
        server_version: format!("vex-serve {}", env!("CARGO_PKG_VERSION")),
        allow_push: !args.read_only,
        user_id: args.user.clone(),
        architur,
    };
    if let Some(user) = &args.user {
        tracing::info!(user, "vex-serve session opened");
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    serve_session(&config, &mut reader, &mut writer).context("serve session")?;
    Ok(())
}
