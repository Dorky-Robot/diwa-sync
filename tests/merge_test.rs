use diwa_sync::merge::merge_into;
use rusqlite::{params, Connection};
use tempfile::TempDir;

const DIWA_SCHEMA: &str = r#"
CREATE TABLE insights (
    id          INTEGER PRIMARY KEY,
    commit_sha  TEXT NOT NULL,
    commit_date TEXT NOT NULL,
    category    TEXT NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    files       TEXT NOT NULL DEFAULT '[]',
    tags        TEXT NOT NULL DEFAULT '',
    source_type TEXT NOT NULL DEFAULT 'git',
    pr_number   INTEGER,
    embedding   BLOB,
    created_at  TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_insights_unique ON insights (commit_sha, title, source_type);
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE VIRTUAL TABLE insights_fts USING fts5(
    title, body, tags,
    content=insights, content_rowid=id
);
"#;

fn make_db(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(DIWA_SCHEMA).unwrap();
    conn
}

fn insert(conn: &Connection, sha: &str, title: &str, body: &str) {
    conn.execute(
        "INSERT INTO insights
         (commit_sha, commit_date, category, title, body, files, tags,
          source_type, pr_number, embedding, created_at)
         VALUES (?1, '2026-01-01', 'feat', ?2, ?3, '[]', '', 'git', NULL, NULL, '2026-01-01')",
        params![sha, title, body],
    )
    .unwrap();
    let rowid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO insights_fts(rowid, title, body, tags) VALUES (?1, ?2, ?3, '')",
        params![rowid, title, body],
    )
    .unwrap();
}

#[test]
fn merge_unions_distinct_rows() {
    let tmp = TempDir::new().unwrap();
    let local_path = tmp.path().join("local.db");
    let peer_path = tmp.path().join("peer.db");

    let local = make_db(&local_path);
    let peer = make_db(&peer_path);

    insert(&local, "aaa", "Local-only commit", "bodyalpha");
    insert(&local, "shared", "Shared commit", "bodyshared");
    insert(&peer, "shared", "Shared commit", "bodyshared");
    insert(&peer, "bbb", "Peer-only commit", "bodybeta");

    drop(local);
    drop(peer);

    let stats = merge_into(&local_path, &peer_path).unwrap();

    assert_eq!(stats.local_before, 2);
    assert_eq!(stats.peer_total, 2);
    assert_eq!(stats.local_after, 3, "should be union of both sides");
    assert_eq!(stats.added(), 1);

    let conn = Connection::open(&local_path).unwrap();
    let titles: Vec<String> = conn
        .prepare("SELECT title FROM insights ORDER BY commit_sha")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        titles,
        vec!["Local-only commit", "Peer-only commit", "Shared commit"]
    );

    // FTS reflects the merged row — search for peer-only body should hit.
    let hit: i64 = conn
        .query_row(
            "SELECT count(*) FROM insights_fts WHERE insights_fts MATCH 'bodybeta'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hit, 1, "FTS rebuilt to include merged peer rows");
}

#[test]
fn merge_idempotent() {
    let tmp = TempDir::new().unwrap();
    let local_path = tmp.path().join("local.db");
    let peer_path = tmp.path().join("peer.db");
    let local = make_db(&local_path);
    let peer = make_db(&peer_path);
    insert(&local, "x", "T", "B");
    insert(&peer, "x", "T", "B");
    drop(local);
    drop(peer);

    let s1 = merge_into(&local_path, &peer_path).unwrap();
    let s2 = merge_into(&local_path, &peer_path).unwrap();
    assert_eq!(s1.added(), 0);
    assert_eq!(s2.added(), 0);
    assert_eq!(s2.local_after, 1);
}

#[test]
fn merge_rejects_missing_dedup_index() {
    // Same schema on both sides but local has no UNIQUE index — would cause
    // unbounded duplicate growth, so merge must refuse.
    let tmp = TempDir::new().unwrap();
    let local_path = tmp.path().join("local.db");
    let peer_path = tmp.path().join("peer.db");

    let no_index_schema = r#"
        CREATE TABLE insights (
            id          INTEGER PRIMARY KEY,
            commit_sha  TEXT NOT NULL,
            commit_date TEXT NOT NULL,
            category    TEXT NOT NULL,
            title       TEXT NOT NULL,
            body        TEXT NOT NULL,
            files       TEXT NOT NULL DEFAULT '[]',
            tags        TEXT NOT NULL DEFAULT '',
            source_type TEXT NOT NULL DEFAULT 'git',
            pr_number   INTEGER,
            embedding   BLOB,
            created_at  TEXT NOT NULL
        );
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE VIRTUAL TABLE insights_fts USING fts5(
            title, body, tags, content=insights, content_rowid=id
        );
    "#;
    Connection::open(&local_path).unwrap().execute_batch(no_index_schema).unwrap();
    let peer = make_db(&peer_path);
    insert(&peer, "x", "T", "B");
    drop(peer);

    let err = merge_into(&local_path, &peer_path).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("UNIQUE index"),
        "expected dedup-index error, got: {msg}"
    );
}

#[test]
fn merge_rejects_schema_mismatch() {
    let tmp = TempDir::new().unwrap();
    let local_path = tmp.path().join("local.db");
    let peer_path = tmp.path().join("peer.db");
    make_db(&local_path);

    // Peer has a divergent schema — missing the `embedding` column.
    let conn = Connection::open(&peer_path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE insights (
            id          INTEGER PRIMARY KEY,
            commit_sha  TEXT NOT NULL,
            commit_date TEXT NOT NULL,
            category    TEXT NOT NULL,
            title       TEXT NOT NULL,
            body        TEXT NOT NULL,
            files       TEXT NOT NULL DEFAULT '[]',
            tags        TEXT NOT NULL DEFAULT '',
            source_type TEXT NOT NULL DEFAULT 'git',
            pr_number   INTEGER,
            created_at  TEXT NOT NULL
        );
        CREATE UNIQUE INDEX idx_insights_unique ON insights (commit_sha, title, source_type);
        CREATE VIRTUAL TABLE insights_fts USING fts5(title, body, tags, content=insights, content_rowid=id);
        "#,
    )
    .unwrap();
    drop(conn);

    let err = merge_into(&local_path, &peer_path).unwrap_err();
    assert!(
        format!("{err:#}").contains("schema mismatch"),
        "expected schema mismatch error, got: {err:#}"
    );
}
