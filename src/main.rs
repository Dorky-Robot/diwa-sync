use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
use std::path::PathBuf;
use tracing::{info, warn};

use diwa_sync::{config, merge, peer, state};

#[derive(Parser, Debug)]
#[command(
    name = "diwa-sync",
    about = "Mesh sync for diwa per-repo SQLite indexes (additive, SSH transport)",
    version
)]
struct Cli {
    /// Plan only — show what would happen, write nothing locally or remotely.
    #[arg(long)]
    dry_run: bool,

    /// Limit to a single peer (matches the SSH alias from config). Repeatable.
    #[arg(long)]
    peer: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Scaffold ~/.diwa-sync/ with a default config.toml.
    Init,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing()?;

    if matches!(cli.command, Some(Command::Init)) {
        return cmd_init();
    }

    run_sync(&cli)
}

fn init_tracing() -> Result<()> {
    let log_dir = config::log_dir()?;
    std::fs::create_dir_all(&log_dir)?;
    // We log to stderr (captured by launchd) AND a per-run file. tracing's
    // default subscriber to stderr is fine; the file is opened up front so we
    // can later cat it for debugging.
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let log_file = log_dir.join(format!("{stamp}.log"));
    let file = std::fs::File::create(&log_file)
        .with_context(|| format!("create log file {}", log_file.display()))?;
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    // We can't keep `guard` alive easily without a global; leak it for the
    // process lifetime. This is fine for a short-lived CLI.
    Box::leak(Box::new(guard));

    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(env)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();
    Ok(())
}

fn cmd_init() -> Result<()> {
    let dir = config::sync_dir()?;
    std::fs::create_dir_all(&dir)?;
    let cfg = config::config_path()?;
    if cfg.exists() {
        info!(path = %cfg.display(), "config already exists, leaving it alone");
    } else {
        config::Config::write_default(&cfg)?;
        info!(path = %cfg.display(), "wrote default config");
    }
    info!(
        "next: edit {} to add SSH peer aliases, then run: diwa-sync --dry-run",
        cfg.display()
    );
    Ok(())
}

fn run_sync(cli: &Cli) -> Result<()> {
    let cfg_path = config::config_path()?;
    if !cfg_path.exists() {
        return Err(anyhow!(
            "no config at {}; run `diwa-sync init` first",
            cfg_path.display()
        ));
    }
    let cfg = config::Config::load(&cfg_path)?;

    let _lock = match acquire_lock() {
        Ok(g) => g,
        Err(e) => {
            warn!("another diwa-sync is running ({e}); exiting cleanly");
            return Ok(());
        }
    };

    info!(
        host = %hostname(),
        diwa_dir = %cfg.diwa_dir.display(),
        peers = ?cfg.peers,
        dry_run = cli.dry_run,
        "diwa-sync start"
    );

    let state_path = config::state_path()?;
    let mut state = state::State::load(&state_path)?;

    let peers: Vec<&String> = if cli.peer.is_empty() {
        cfg.peers.iter().collect()
    } else {
        cfg.peers.iter().filter(|p| cli.peer.contains(p)).collect()
    };
    if peers.is_empty() {
        warn!("no peers selected (config has none, or --peer didn't match)");
        return Ok(());
    }

    let staging = tempfile::tempdir().context("create staging dir")?;

    let mut totals = Totals::default();

    for host in peers {
        info!(peer = %host, "--- peer ---");
        if !peer::is_reachable(host) {
            warn!(peer = %host, "unreachable, skipping");
            totals.peers_skipped += 1;
            continue;
        }

        let local_keys = list_local_repos(&cfg.diwa_dir)?;
        let remote_keys = match peer::list_repos(host) {
            Ok(k) => k,
            Err(e) => {
                warn!(peer = %host, error = %e, "list_repos failed, skipping peer");
                totals.peers_skipped += 1;
                continue;
            }
        };
        let union: BTreeSet<String> =
            local_keys.iter().chain(remote_keys.iter()).cloned().collect();

        for key in &union {
            let in_local = local_keys.contains(key);
            let in_peer = remote_keys.contains(key);
            match (in_local, in_peer) {
                (true, true) => {
                    if let Err(e) =
                        sync_existing(host, key, &cfg.diwa_dir, staging.path(), cli.dry_run, &mut state)
                    {
                        warn!(peer = %host, repo = %key, error = %e, "merge failed");
                        totals.errors += 1;
                    } else {
                        totals.merged += 1;
                    }
                }
                (false, true) => {
                    if let Err(e) = pull_init(host, key, &cfg.diwa_dir, staging.path(), cli.dry_run)
                    {
                        warn!(peer = %host, repo = %key, error = %e, "pull-init failed");
                        totals.errors += 1;
                    } else {
                        totals.pulled += 1;
                    }
                }
                (true, false) => {
                    if let Err(e) = push_seed(host, key, &cfg.diwa_dir, staging.path(), cli.dry_run)
                    {
                        warn!(peer = %host, repo = %key, error = %e, "push-seed failed");
                        totals.errors += 1;
                    } else {
                        totals.pushed += 1;
                    }
                }
                (false, false) => unreachable!(),
            }
        }
    }

    if !cli.dry_run {
        state.save(&state_path)?;
    }

    info!(
        merged = totals.merged,
        pulled = totals.pulled,
        pushed = totals.pushed,
        peers_skipped = totals.peers_skipped,
        errors = totals.errors,
        "diwa-sync done"
    );
    Ok(())
}

#[derive(Default)]
struct Totals {
    merged: u32,
    pulled: u32,
    pushed: u32,
    peers_skipped: u32,
    errors: u32,
}

fn list_local_repos(diwa_dir: &std::path::Path) -> Result<Vec<String>> {
    if !diwa_dir.exists() {
        return Ok(vec![]);
    }
    let mut keys = vec![];
    for entry in std::fs::read_dir(diwa_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if peer::validate_repo_key(&name).is_err() {
            continue;
        }
        let db = entry.path().join("index.db");
        if db.is_file() && std::fs::metadata(&db)?.len() > 0 {
            keys.push(name);
        }
    }
    keys.sort();
    Ok(keys)
}

fn sync_existing(
    host: &str,
    key: &str,
    diwa_dir: &std::path::Path,
    staging: &std::path::Path,
    dry_run: bool,
    state: &mut state::State,
) -> Result<()> {
    let local_db = diwa_dir.join(key).join("index.db");
    let stage = staging.join(format!("{host}-{key}.peer.db"));
    peer::snapshot(host, key, &stage)?;

    if dry_run {
        // Dry run: do the merge against a *copy* of the local DB so we report
        // what would change without touching the real file.
        let local_copy = staging.join(format!("{host}-{key}.local.db"));
        peer::local_snapshot(&local_db, &local_copy)?;
        let stats = merge::merge_into(&local_copy, &stage)?;
        info!(
            peer = %host,
            repo = %key,
            local_before = stats.local_before,
            peer_total = stats.peer_total,
            would_add = stats.added(),
            "[dry-run] merge plan"
        );
        return Ok(());
    }

    let stats = merge::merge_into(&local_db, &stage)?;
    info!(
        peer = %host,
        repo = %key,
        local_before = stats.local_before,
        peer_total = stats.peer_total,
        added = stats.added(),
        local_after = stats.local_after,
        "merged"
    );
    state.record(host, key, stats.local_after);
    Ok(())
}

fn pull_init(
    host: &str,
    key: &str,
    diwa_dir: &std::path::Path,
    staging: &std::path::Path,
    dry_run: bool,
) -> Result<()> {
    info!(peer = %host, repo = %key, "peer has repo we lack");
    if dry_run {
        info!(peer = %host, repo = %key, "[dry-run] would pull-init from peer");
        return Ok(());
    }
    let stage = staging.join(format!("{host}-{key}.peer.db"));
    peer::snapshot(host, key, &stage)?;
    let dst_dir = diwa_dir.join(key);
    std::fs::create_dir_all(&dst_dir)?;
    let dst = dst_dir.join("index.db");
    std::fs::copy(&stage, &dst)
        .with_context(|| format!("copy {} → {}", stage.display(), dst.display()))?;
    info!(peer = %host, repo = %key, "pull-init complete");
    Ok(())
}

fn push_seed(
    host: &str,
    key: &str,
    diwa_dir: &std::path::Path,
    staging: &std::path::Path,
    dry_run: bool,
) -> Result<()> {
    info!(peer = %host, repo = %key, "we have repo peer lacks");
    if dry_run {
        info!(peer = %host, repo = %key, "[dry-run] would push-seed to peer");
        return Ok(());
    }
    let local_db = diwa_dir.join(key).join("index.db");
    let stage = staging.join(format!("{host}-{key}.local-seed.db"));
    peer::local_snapshot(&local_db, &stage)?;
    peer::push_seed(host, key, &stage)?;
    info!(peer = %host, repo = %key, "push-seed complete");
    Ok(())
}

/// Single-instance lock via mkdir (atomic on POSIX). Returns a guard that
/// removes the dir on drop.
fn acquire_lock() -> Result<LockGuard> {
    let dir = config::lock_dir()?;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::create_dir(&dir) {
        Ok(()) => Ok(LockGuard { path: dir }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(anyhow!("lock {} already held", dir.display()))
        }
        Err(e) => Err(e.into()),
    }
}

struct LockGuard {
    path: PathBuf,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

fn hostname() -> String {
    std::process::Command::new("/bin/hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
