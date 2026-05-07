#!/usr/bin/env bash
# Deploy diwa-sync to a peer Mac on an SSH mesh.
#
# Usage: deploy.sh <ssh-alias> "<peer1> <peer2> ..."
#
# Example: ./deploy.sh host-b "host-a host-c"
#   sets up host-b with host-a and host-c as its sync peers.
#
# What this does on the target:
#   1. rsync this crate to ~/Projects/diwa-sync/
#   2. cargo build --release
#   3. install the binary to ~/.local/bin/diwa-sync
#   4. write ~/.diwa-sync/config.toml with the supplied peer list
#   5. write ~/Library/LaunchAgents/diwa-sync.plist (NOT loaded — verify
#      with `~/.local/bin/diwa-sync --dry-run` on the peer first, then
#      `launchctl load ~/Library/LaunchAgents/diwa-sync.plist`)

set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <ssh-alias> \"<peer1> <peer2> ...\"" >&2
  exit 1
fi

HOST="$1"
PEERS_RAW="$2"

CRATE_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== deploying diwa-sync to $HOST ==="
echo "  source crate: $CRATE_DIR"
echo "  peer list:    $PEERS_RAW"

# Resolve peer's $HOME so the LaunchAgent plist gets absolute paths
# without any local-machine assumption.
REMOTE_HOME="$(ssh "$HOST" 'printf %s "$HOME"')"
if [[ -z "$REMOTE_HOME" || "$REMOTE_HOME" != /* ]]; then
  echo "ERROR: could not resolve remote \$HOME on $HOST (got '$REMOTE_HOME')" >&2
  exit 1
fi
echo "  remote HOME:  $REMOTE_HOME"

# 1. rsync source. Exclude target/ and .git/ to keep the transfer small
# and avoid leaking commit history we may not have cleaned for that peer.
echo "[1/5] rsync source to $HOST:Projects/diwa-sync/"
ssh "$HOST" 'mkdir -p "$HOME/Projects/diwa-sync"'
rsync -a --delete \
  --exclude target/ \
  --exclude .git/ \
  "$CRATE_DIR/" "$HOST:Projects/diwa-sync/"

# 2. cargo build --release on the peer (gets correct arch).
echo "[2/5] cargo build --release on $HOST (this may take a few minutes)"
ssh "$HOST" 'cd "$HOME/Projects/diwa-sync" && cargo build --release'

# 3. install binary.
echo "[3/5] install binary to $HOST:.local/bin/diwa-sync"
ssh "$HOST" 'mkdir -p "$HOME/.local/bin" && cp "$HOME/Projects/diwa-sync/target/release/diwa-sync" "$HOME/.local/bin/diwa-sync"'

# 4. write config.toml with the supplied peer list.
echo "[4/5] write ~/.diwa-sync/config.toml on $HOST"
peers_toml=""
for p in $PEERS_RAW; do
  if [[ -n "$peers_toml" ]]; then peers_toml+=", "; fi
  peers_toml+="\"$p\""
done
ssh "$HOST" 'mkdir -p "$HOME/.diwa-sync"'
ssh "$HOST" 'cat > "$HOME/.diwa-sync/config.toml"' <<EOF
# diwa-sync config (deployed by deploy.sh)
peers = [$peers_toml]
EOF

# 5. write LaunchAgent plist. We substitute the remote $HOME so the plist
# carries no assumption about the deployer's username or home path.
echo "[5/5] write LaunchAgent plist on $HOST"
ssh "$HOST" 'cat > "$HOME/Library/LaunchAgents/diwa-sync.plist"' <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>diwa-sync</string>
    <key>ProgramArguments</key>
    <array>
        <string>$REMOTE_HOME/.local/bin/diwa-sync</string>
    </array>
    <key>StartCalendarInterval</key>
    <array>
        <dict><key>Minute</key><integer>4</integer></dict>
        <dict><key>Minute</key><integer>14</integer></dict>
        <dict><key>Minute</key><integer>24</integer></dict>
        <dict><key>Minute</key><integer>34</integer></dict>
        <dict><key>Minute</key><integer>44</integer></dict>
        <dict><key>Minute</key><integer>54</integer></dict>
    </array>
    <key>RunAtLoad</key>
    <false/>
    <key>StandardOutPath</key>
    <string>$REMOTE_HOME/.diwa-sync/log/launchd.out.log</string>
    <key>StandardErrorPath</key>
    <string>$REMOTE_HOME/.diwa-sync/log/launchd.err.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$REMOTE_HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
        <key>HOME</key>
        <string>$REMOTE_HOME</string>
    </dict>
    <key>WorkingDirectory</key>
    <string>$REMOTE_HOME</string>
</dict>
</plist>
EOF

echo
echo "=== $HOST: deployment complete ==="
echo "Verify on $HOST:"
echo "  ssh $HOST '~/.local/bin/diwa-sync --dry-run'"
echo "Then load the LaunchAgent on $HOST:"
echo "  ssh $HOST 'launchctl load ~/Library/LaunchAgents/diwa-sync.plist'"
