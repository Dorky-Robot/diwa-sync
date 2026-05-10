# diwa-sync

Mesh sync for [diwa](https://github.com/Dorky-Robot/diwa) per-repo SQLite indexes. Each machine on a small SSH mesh runs the same one-shot job on a cron, pulls SQLite-consistent snapshots from its peers, and merges them into the local index additively.

## Why

`diwa` indexes git history into a per-repo SQLite database at `~/.diwa/<slug>/index.db`. On a single machine that's fine — but if you work across multiple machines, each has its own partial corpus. `diwa-sync` keeps every machine's index in lockstep without a central server: laptops come and go, and the union just propagates.

## How it works

For each peer, for each repo present on either side, every cron tick:

1. Take a SQLite-consistent snapshot of the peer's `index.db` via `sqlite3 .backup` — safe even if the peer's diwa daemon is mid-write.
2. Pull the snapshot to a local staging dir via `rsync`.
3. Open the local DB with a 30s busy timeout, `ATTACH` the staged peer DB, verify schemas match, then `INSERT OR IGNORE` rows from peer into local. The unique index `(commit_sha, title, source_type)` deduplicates without losing any row that exists only on one side.
4. If new rows were merged, rebuild the FTS5 index.

The sync is **pull-only in steady state** — each machine only ever writes to its own DB. The one exception is bootstrap: if a peer is missing a repo entirely, a one-shot snapshot is pushed to the peer's empty directory; from then on it's pull-only for that repo. After every machine has run its cron once, all participants converge to the union of every corpus.

## Requirements

- macOS (LaunchAgent integration; the binary itself is portable but install/cron is Mac-specific).
- Rust toolchain.
- `sqlite3`, `ssh`, `rsync` on every machine.
- Passwordless SSH between every pair of peers (`ssh -o BatchMode=yes <alias> true` must succeed).
- `diwa` installed on at least one machine.

## Install (on each machine)

```sh
git clone https://github.com/Dorky-Robot/diwa-sync
cd diwa-sync
cargo build --release
mkdir -p ~/.local/bin && cp target/release/diwa-sync ~/.local/bin/
diwa-sync init                         # writes ~/.diwa-sync/config.toml
$EDITOR ~/.diwa-sync/config.toml       # add SSH aliases of the OTHER machines
diwa-sync --dry-run                    # plan-only; safe
```

`config.toml`:

```toml
peers = ["host-a", "host-b"]
# diwa_dir = "/path/to/.diwa"  # default: ~/.diwa
```

## Deploy from one machine to a peer

Once one machine is set up, bootstrapping a peer is one command:

```sh
./deploy.sh <peer-ssh-alias> "<other-peer-1> <other-peer-2>"
```

This rsyncs the source, builds release on the target, installs the binary, writes a peer-specific `config.toml`, and writes the LaunchAgent plist using the *target's* `$HOME` (no deployer-specific paths leak in). It does **not** load the LaunchAgent — verify on the peer first:

```sh
ssh <peer> '~/.local/bin/diwa-sync --dry-run'
ssh <peer> 'launchctl load ~/Library/LaunchAgents/diwa-sync.plist'
```

## Schedule

The default plist fires at `:04`, `:14`, `:24`, `:34`, `:44`, `:54` of each hour. A single-instance lock at `~/.diwa-sync/lock.d/` prevents overlap if a previous run is still in flight.

## Operations

| Want to… | Run |
|----------|-----|
| Plan without writing | `diwa-sync --dry-run` |
| Limit to one peer | `diwa-sync --peer host-a` |
| Inspect a run | `~/.diwa-sync/log/<timestamp>.log` (per-run) and `launchd.{out,err}.log` |
| Stop the cron | `launchctl unload ~/Library/LaunchAgents/diwa-sync.plist` |

Two schema problems are detected pre-merge and surface as **clear, skipped-with-error** outcomes rather than silent corruption:

- **Schema mismatch** between local and peer (e.g. one machine on a newer diwa with a new column) — search logs for `schema mismatch on \`insights\``.
- **Missing dedup index** on the local DB — without a `UNIQUE INDEX` over `(commit_sha, title, source_type)`, `INSERT OR IGNORE` has nothing to dedupe against and would append peer rows on every tick. The error tells you exactly what to run: `CREATE UNIQUE INDEX idx_insights_unique ON insights (commit_sha, title, source_type);`

## What is NOT synced

- `~/.diwa/repos.json` — maps slug → local worktree path; intentionally machine-local.
- `~/.diwa/queue/` — ephemeral indexer queue.
- `~/.diwa/daemon.log` — per-host log.
- `~/.diwa/models/` — embedding model files; copy once manually if you're bootstrapping a new machine.

## Search across the mesh

Once `diwa-sync` is running, `diwa search <slug> "..."` works on any machine for any synced corpus — `diwa` resolves the slug by scanning `~/.diwa/` directory entries, so no per-machine "registration" is needed for search. (`diwa update` / reindexing still needs a local worktree path in `repos.json`; `diwa-sync` deliberately leaves that file alone.)

## Tests

```sh
cargo test
```

Three tests cover the merge path: union of distinct rows, idempotency of repeated merges, and rejection of schema mismatches.

## License

MIT OR Apache-2.0.
