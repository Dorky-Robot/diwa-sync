use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use std::path::Path;

/// Columns of `insights` we copy from peer → local. `id` is autoincrement on
/// the receiving side. The dedup key is the unique index `(commit_sha, title,
/// source_type)`, so `INSERT OR IGNORE` collapses duplicates without losing
/// rows that exist on only one side.
const INSIGHT_COLS: &[&str] = &[
    "commit_sha",
    "commit_date",
    "category",
    "title",
    "body",
    "files",
    "tags",
    "source_type",
    "pr_number",
    "embedding",
    "created_at",
];

#[derive(Debug, Clone, Copy)]
pub struct MergeStats {
    pub local_before: i64,
    pub peer_total: i64,
    pub local_after: i64,
}

impl MergeStats {
    pub fn added(&self) -> i64 {
        self.local_after - self.local_before
    }
}

/// Merge `peer_db` (a snapshot of a peer's `index.db`) into `local_db` in place.
///
/// Safe to run while diwa's daemon may be writing: we use `BEGIN IMMEDIATE`
/// with a 30s busy timeout, so we either get the write lock or fail cleanly.
pub fn merge_into(local_db: &Path, peer_db: &Path) -> Result<MergeStats> {
    let conn = Connection::open(local_db)
        .with_context(|| format!("open local db {}", local_db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;

    let peer_db_str = peer_db
        .to_str()
        .ok_or_else(|| anyhow!("peer db path is not valid UTF-8"))?;

    conn.execute_batch(&format!("ATTACH DATABASE '{}' AS peer;", escape_sql(peer_db_str)))
        .context("ATTACH peer database")?;

    schema_compatible(&conn).context("schema compatibility check")?;
    require_dedup_index(&conn).context("dedup index check")?;

    let local_before: i64 =
        conn.query_row("SELECT count(*) FROM insights", [], |r| r.get(0))?;
    let peer_total: i64 =
        conn.query_row("SELECT count(*) FROM peer.insights", [], |r| r.get(0))?;

    let cols = INSIGHT_COLS.join(", ");
    let sql = format!(
        "INSERT OR IGNORE INTO insights ({cols}) SELECT {cols} FROM peer.insights"
    );

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let inserted = conn.execute(&sql, [])?;
    conn.execute_batch("COMMIT;")?;

    let local_after: i64 =
        conn.query_row("SELECT count(*) FROM insights", [], |r| r.get(0))?;

    if inserted > 0 {
        conn.execute_batch("INSERT INTO insights_fts(insights_fts) VALUES('rebuild');")
            .context("rebuild FTS")?;
    }

    conn.execute_batch("DETACH DATABASE peer;").ok();

    Ok(MergeStats {
        local_before,
        peer_total,
        local_after,
    })
}

/// Verify that local and peer have the same `insights` schema. We hardcode our
/// column list, so a divergent column set would silently drop data — better to
/// fail loud and skip the repo than to merge a partial row.
fn schema_compatible(conn: &Connection) -> Result<()> {
    let local_cols = table_columns(conn, "main", "insights")?;
    let peer_cols = table_columns(conn, "peer", "insights")?;

    if local_cols != peer_cols {
        return Err(anyhow!(
            "schema mismatch on `insights`: local={:?}, peer={:?}",
            local_cols,
            peer_cols
        ));
    }

    let expected: std::collections::BTreeSet<&str> =
        INSIGHT_COLS.iter().copied().collect();
    let actual: std::collections::BTreeSet<&str> =
        local_cols.iter().map(|s| s.as_str()).collect();
    let missing: Vec<&&str> = expected.difference(&actual).collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "diwa schema is missing expected columns we copy: {:?}. \
             diwa-sync needs an update.",
            missing
        ));
    }
    Ok(())
}

/// Refuse to merge if the local `insights` table has no UNIQUE index covering
/// `(commit_sha, title, source_type)`. Without it, `INSERT OR IGNORE` has no
/// constraint to dedupe against and every peer row gets blindly appended,
/// causing unbounded growth on every tick. (Discovered in the wild on a repo
/// whose schema was created before diwa added the migration; see git log.)
fn require_dedup_index(conn: &Connection) -> Result<()> {
    // Look for a unique index on `insights` that covers exactly the dedup
    // columns. SQLite may give it any name (we use `idx_insights_unique` by
    // convention but tolerate alternatives).
    let mut stmt = conn.prepare(
        "SELECT name FROM main.sqlite_master \
         WHERE type='index' AND tbl_name='insights'",
    )?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;

    let dedup_cols: std::collections::BTreeSet<&str> =
        ["commit_sha", "title", "source_type"].into_iter().collect();

    for name in &names {
        // PRAGMA index_info → cid, seqno, name (column name)
        let mut info = conn.prepare(&format!("PRAGMA main.index_info('{}')", name))?;
        let cols: std::collections::BTreeSet<String> = info
            .query_map([], |r| r.get::<_, String>(2))?
            .collect::<rusqlite::Result<_>>()?;

        // PRAGMA index_list reveals whether this index is UNIQUE.
        let mut list = conn.prepare("PRAGMA main.index_list('insights')")?;
        let unique: bool = list
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?
            .filter_map(Result::ok)
            .any(|(n, u)| n == *name && u == 1);

        if unique
            && cols.len() == dedup_cols.len()
            && cols.iter().map(|s| s.as_str()).collect::<std::collections::BTreeSet<_>>()
                == dedup_cols
        {
            return Ok(());
        }
    }

    Err(anyhow!(
        "local `insights` table is missing a UNIQUE index over \
         (commit_sha, title, source_type) — INSERT OR IGNORE has nothing to \
         dedupe against, so merging would corrupt the DB with unbounded duplicates. \
         Run: sqlite3 <path> \"CREATE UNIQUE INDEX idx_insights_unique ON insights \
         (commit_sha, title, source_type);\" (deduplicate first if needed)."
    ))
}

fn table_columns(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>> {
    // Identifiers are validated (no injection): callers only pass `main`,
    // `peer`, and `insights`. We still gate on the alnum+underscore charset.
    if !schema.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(anyhow!("invalid schema identifier: {schema}"));
    }
    if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(anyhow!("invalid table identifier: {table}"));
    }
    // We use the *non-virtual* PRAGMA form which reliably honors the schema
    // qualifier; the table-valued `pragma_table_info()` does not in all builds.
    let sql = format!("PRAGMA {schema}.table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    // PRAGMA table_info columns: cid(0), name(1), type(2), notnull(3),
    // dflt_value(4), pk(5).
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    if cols.is_empty() {
        return Err(anyhow!("table `{schema}.{table}` not found or empty schema"));
    }
    Ok(cols)
}

fn escape_sql(s: &str) -> String {
    s.replace('\'', "''")
}
