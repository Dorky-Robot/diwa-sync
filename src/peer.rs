use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Hardcoded SSH options for every connection.
/// - BatchMode rejects password prompts (fast failure, not hangs).
/// - ConnectTimeout caps DNS+TCP.
/// - ControlMaster + ControlPersist multiplex repeated connections to the
///   same host through a single TCP/auth session, which collapses ~144
///   handshakes/tick down to ~2. Saves ~30s per tick at our scale.
fn ssh_args(host: &str) -> Vec<String> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=60s".into(),
        // %C is a hash of host+port+user — collision-free per peer.
        "-o".into(),
        "ControlPath=~/.ssh/diwa-sync-cm-%C".into(),
        host.into(),
    ]
}

pub fn is_reachable(host: &str) -> bool {
    let mut args = ssh_args(host);
    args.push("true".into());
    Command::new("/usr/bin/ssh")
        .args(&args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Validate a diwa repo key. Diwa derives keys from git remotes via a `--`
/// separator (e.g. `Dorky-Robot--humOS`), so the safe charset is alnum + dot
/// + dash + underscore. Anything else is rejected so it can't sneak into a
/// shell command we build below.
pub fn validate_repo_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 200 {
        bail!("invalid repo key length: {:?}", key);
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("invalid characters in repo key: {:?}", key);
    }
    Ok(())
}

/// Return repo keys on `host` that have a non-empty `index.db`.
pub fn list_repos(host: &str) -> Result<Vec<String>> {
    let mut args = ssh_args(host);
    // `find` is more reliable than ls-piped-through-shell-loops across distros.
    // We list directories one level down that contain a non-empty index.db.
    args.push(
        r#"cd "$HOME/.diwa" 2>/dev/null && \
           /usr/bin/find . -mindepth 2 -maxdepth 2 -type f -name index.db -size +0c \
             -exec dirname {} \; | sed 's|^\./||'"#
            .into(),
    );
    let out = Command::new("/usr/bin/ssh")
        .args(&args)
        .output()
        .context("ssh list_repos")?;
    if !out.status.success() {
        return Err(anyhow!(
            "ssh list_repos on {host} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let mut keys: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    keys.retain(|k| validate_repo_key(k).is_ok());
    keys.sort();
    Ok(keys)
}

/// Pull a fresh SQLite-consistent snapshot of `~/.diwa/{repo_key}/index.db`
/// from `host` to `dest`. Uses `sqlite3 .backup` so the snapshot is consistent
/// even if diwa's daemon is mid-write on the peer.
pub fn snapshot(host: &str, repo_key: &str, dest: &Path) -> Result<()> {
    validate_repo_key(repo_key)?;

    // Step 1: ask peer to produce a snapshot in its /tmp, echo back the path.
    // -cmd "PRAGMA busy_timeout=30000" widens the .backup wait from the
    // default ~5s to 30s, eliminating the rare "database is locked" we saw
    // when a peer's diwa daemon was actively writing.
    let remote_cmd = format!(
        r#"set -e
tmp=$(mktemp /tmp/diwa-sync-snap.XXXXXX) || exit 1
trap 'rm -f "$tmp"' ERR
sqlite3 -cmd "PRAGMA busy_timeout=30000" "$HOME/.diwa/{key}/index.db" ".backup '$tmp'" >/dev/null
echo "$tmp"
"#,
        key = repo_key
    );
    let mut args = ssh_args(host);
    args.push(remote_cmd);
    let out = Command::new("/usr/bin/ssh")
        .args(&args)
        .output()
        .context("ssh snapshot create")?;
    if !out.status.success() {
        return Err(anyhow!(
            "snapshot create on {host}:{repo_key} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let remote_tmp = String::from_utf8(out.stdout)
        .context("snapshot path not utf8")?
        .trim()
        .to_string();
    if remote_tmp.is_empty() || !remote_tmp.starts_with("/tmp/diwa-sync-snap.") {
        return Err(anyhow!(
            "unexpected snapshot path from {host}: {:?}",
            remote_tmp
        ));
    }

    // Step 2: rsync the snapshot down. We don't use ssh ControlMaster here;
    // each call is a fresh ssh, which is fine at our scale (≤20 repos × peers).
    let rsync_src = format!("{host}:{remote_tmp}");
    let dest_str = dest
        .to_str()
        .ok_or_else(|| anyhow!("dest path not utf8: {}", dest.display()))?;
    let status = Command::new("/usr/bin/rsync")
        .args(["-a", "--timeout=30", &rsync_src, dest_str])
        .status()
        .context("rsync snapshot")?;
    if !status.success() {
        // Try to clean up the remote tmp anyway.
        let _ = remote_rm(host, &remote_tmp);
        return Err(anyhow!("rsync from {rsync_src} → {dest_str} failed: {status}"));
    }

    // Step 3: best-effort cleanup of the peer's /tmp.
    let _ = remote_rm(host, &remote_tmp);
    Ok(())
}

fn remote_rm(host: &str, remote_path: &str) -> Result<()> {
    if !remote_path.starts_with("/tmp/diwa-sync-snap.") {
        bail!("refusing to rm non-snapshot path: {remote_path}");
    }
    let mut args = ssh_args(host);
    args.push(format!("rm -f {}", shell_quote(remote_path)));
    Command::new("/usr/bin/ssh")
        .args(&args)
        .status()
        .context("ssh remote_rm")?;
    Ok(())
}

fn shell_quote(s: &str) -> String {
    // Single-quote everything; escape embedded single quotes the POSIX way.
    let escaped = s.replace('\'', r"'\''");
    format!("'{escaped}'")
}

/// Push a local file to `host:~/.diwa/{repo_key}/index.db`, creating the dir
/// if needed. Used to seed a peer that doesn't yet have a repo.
pub fn push_seed(host: &str, repo_key: &str, src: &Path) -> Result<()> {
    validate_repo_key(repo_key)?;
    let mut args = ssh_args(host);
    args.push(format!(
        "mkdir -p \"$HOME/.diwa/{key}\"",
        key = repo_key
    ));
    let status = Command::new("/usr/bin/ssh")
        .args(&args)
        .status()
        .context("ssh mkdir for push_seed")?;
    if !status.success() {
        return Err(anyhow!("mkdir on {host}:.diwa/{repo_key} failed"));
    }
    let src_str = src
        .to_str()
        .ok_or_else(|| anyhow!("src path not utf8: {}", src.display()))?;
    let rsync_dest = format!("{host}:.diwa/{repo_key}/index.db");
    let status = Command::new("/usr/bin/rsync")
        .args(["-a", "--timeout=30", src_str, &rsync_dest])
        .status()
        .context("rsync push_seed")?;
    if !status.success() {
        return Err(anyhow!("rsync {src_str} → {rsync_dest} failed: {status}"));
    }
    Ok(())
}

/// Take a SQLite-consistent local snapshot via `sqlite3 .backup`. We never
/// rsync the live `index.db`; we always go through a snapshot so we can't
/// observe a torn write.
pub fn local_snapshot(local_db: &Path, dest: &Path) -> Result<()> {
    let local_str = local_db
        .to_str()
        .ok_or_else(|| anyhow!("local db path not utf8: {}", local_db.display()))?;
    let dest_str = dest
        .to_str()
        .ok_or_else(|| anyhow!("dest path not utf8: {}", dest.display()))?;
    // Single backslash-escape for embedded apostrophes is enough — paths under
    // our control, but we still build the SQL through the CLI carefully.
    let backup_sql = format!(".backup '{}'", dest_str.replace('\'', "''"));
    let status = Command::new("/usr/bin/sqlite3")
        .arg("-cmd")
        .arg("PRAGMA busy_timeout=30000")
        .arg(local_str)
        .arg(&backup_sql)
        .status()
        .context("sqlite3 .backup local")?;
    if !status.success() {
        return Err(anyhow!("local sqlite3 .backup failed: {status}"));
    }
    Ok(())
}

#[allow(dead_code)]
const _DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
