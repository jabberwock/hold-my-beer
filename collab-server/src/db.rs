use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

pub async fn init_db() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite:collab.db")?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS messages (
            id TEXT PRIMARY KEY,
            hash TEXT NOT NULL,
            sender TEXT NOT NULL,
            recipient TEXT NOT NULL,
            content TEXT NOT NULL,
            refs TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            read_at TEXT
        )
        "#,
    )
    .execute(&pool)
    .await?;

    // Migrate existing installs — ignored if column already exists.
    let _ = sqlx::query("ALTER TABLE messages ADD COLUMN read_at TEXT")
        .execute(&pool)
        .await;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_recipient_timestamp
        ON messages(recipient, timestamp DESC)
        "#,
    )
    .execute(&pool)
    .await?;

    // presence has no PRIMARY KEY on instance_id: uniqueness is enforced via
    // (team_id, instance_id) by an expression index in apply_team_schema,
    // so two teams can both have a "webdev" entry.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS presence (
            instance_id TEXT NOT NULL,
            role TEXT NOT NULL DEFAULT '',
            last_seen TEXT NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS todos (
            id TEXT PRIMARY KEY,
            hash TEXT NOT NULL,
            instance TEXT NOT NULL,
            assigned_by TEXT NOT NULL,
            description TEXT NOT NULL,
            created_at TEXT NOT NULL,
            completed_at TEXT
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_todos_instance
        ON todos(instance, created_at DESC)
        "#,
    )
    .execute(&pool)
    .await?;

    apply_team_schema(&pool).await?;

    Ok(pool)
}

/// Team-era schema. Additive over the legacy schema: every new column is
/// nullable so pre-team rows keep working (team_id NULL = legacy namespace).
/// Safe to call repeatedly — column adds swallow the "duplicate column" error
/// and every CREATE uses IF NOT EXISTS.
async fn apply_team_schema(pool: &SqlitePool) -> Result<()> {
    // Add team_id to the three per-traffic tables.
    let _ = sqlx::query("ALTER TABLE messages ADD COLUMN team_id TEXT")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE presence ADD COLUMN team_id TEXT")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE todos ADD COLUMN team_id TEXT")
        .execute(pool)
        .await;

    // Pre-team installs declared `instance_id` as the presence PRIMARY KEY,
    // which makes cross-team name reuse impossible. Rebuild the table to
    // drop that constraint. Brand-new DBs already ship without the PK (see
    // init_db), so this branch is a no-op for them.
    let instance_id_is_pk: Option<i64> = sqlx::query_scalar(
        "SELECT pk FROM pragma_table_info('presence') WHERE name = 'instance_id'",
    )
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    if instance_id_is_pk.unwrap_or(0) > 0 {
        sqlx::query(
            r#"CREATE TABLE presence_rebuild (
                instance_id TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT '',
                last_seen TEXT NOT NULL,
                team_id TEXT
            )"#,
        )
        .execute(pool)
        .await?;
        sqlx::query(
            r#"INSERT INTO presence_rebuild (instance_id, role, last_seen, team_id)
               SELECT instance_id, role, last_seen, team_id FROM presence"#,
        )
        .execute(pool)
        .await?;
        sqlx::query("DROP TABLE presence").execute(pool).await?;
        sqlx::query("ALTER TABLE presence_rebuild RENAME TO presence")
            .execute(pool)
            .await?;
    }

    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_presence_identity
        ON presence(COALESCE(team_id, ''), instance_id)
        "#,
    )
    .execute(pool)
    .await?;

    // team_id-aware indexes. The legacy indexes stay — they still serve
    // queries against NULL team_id, and SQLite's planner will pick whichever
    // is cheapest.
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_team_recipient_ts
        ON messages(team_id, recipient, timestamp DESC)
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_todos_team_instance
        ON todos(team_id, instance, created_at DESC)
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS teams (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Tokens are stored as SHA-256 hashes; plaintext is shown to the human
    // once at mint time and never again. A team can have multiple live
    // tokens during a rotation grace window — enforced by both rows being
    // non-revoked (revoked_at IS NULL). Rotation marks the old one revoked.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS team_tokens (
            token_hash TEXT PRIMARY KEY,
            team_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            revoked_at TEXT,
            FOREIGN KEY(team_id) REFERENCES teams(id)
        )
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_team_tokens_team
        ON team_tokens(team_id) WHERE revoked_at IS NULL
        "#,
    )
    .execute(pool)
    .await?;

    // Singleton lease per (team_id, instance_id). team_id may be NULL
    // (legacy mode — the lease still protects against duplicate workers
    // even without team scoping). SQLite NULLs don't compare equal in a
    // UNIQUE constraint, so we enforce uniqueness with an expression index
    // that coalesces NULL to a sentinel string.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS worker_leases (
            team_id TEXT,
            instance_id TEXT NOT NULL,
            pid INTEGER NOT NULL,
            host TEXT NOT NULL,
            acquired_at TEXT NOT NULL,
            last_heartbeat TEXT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_worker_leases_identity
        ON worker_leases(COALESCE(team_id, ''), instance_id)
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_worker_leases_heartbeat
        ON worker_leases(last_heartbeat)
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn init_test_db() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    sqlx::query(
        r#"
        CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            hash TEXT NOT NULL,
            sender TEXT NOT NULL,
            recipient TEXT NOT NULL,
            content TEXT NOT NULL,
            refs TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            read_at TEXT
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX idx_recipient_timestamp
        ON messages(recipient, timestamp DESC)
        "#,
    )
    .execute(&pool)
    .await?;

    // presence has no PRIMARY KEY on instance_id: uniqueness is enforced via
    // (team_id, instance_id) by an expression index in apply_team_schema,
    // so two teams can both have a "webdev" entry.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS presence (
            instance_id TEXT NOT NULL,
            role TEXT NOT NULL DEFAULT '',
            last_seen TEXT NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS todos (
            id TEXT PRIMARY KEY,
            hash TEXT NOT NULL,
            instance TEXT NOT NULL,
            assigned_by TEXT NOT NULL,
            description TEXT NOT NULL,
            created_at TEXT NOT NULL,
            completed_at TEXT
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_todos_instance
        ON todos(instance, created_at DESC)
        "#,
    )
    .execute(&pool)
    .await?;

    apply_team_schema(&pool).await?;

    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    #[tokio::test]
    async fn test_init_test_db() {
        let pool = init_test_db().await.unwrap();

        let result = sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name='messages'")
            .fetch_one(&pool)
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_insert_and_query_message() {
        let pool = init_test_db().await.unwrap();

        sqlx::query(
            "INSERT INTO messages (id, hash, sender, recipient, content, refs, timestamp) VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind("test-id")
        .bind("test-hash")
        .bind("worker1")
        .bind("worker2")
        .bind("test content")
        .bind("")
        .bind("2024-03-27T14:30:45Z")
        .execute(&pool)
        .await
        .unwrap();

        let row = sqlx::query("SELECT * FROM messages WHERE id = ?")
            .bind("test-id")
            .fetch_one(&pool)
            .await
            .unwrap();

        let sender: String = row.get("sender");
        assert_eq!(sender, "worker1");
    }

    #[tokio::test]
    async fn test_presence_table_exists() {
        let pool = init_test_db().await.unwrap();

        let result = sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name='presence'")
            .fetch_one(&pool)
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn team_schema_creates_teams_tokens_leases_tables() {
        let pool = init_test_db().await.unwrap();

        for table in ["teams", "team_tokens", "worker_leases"] {
            let row = sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_optional(&pool)
                .await
                .unwrap();
            assert!(row.is_some(), "missing team-era table: {}", table);
        }
    }

    #[tokio::test]
    async fn team_schema_adds_team_id_column_to_legacy_tables() {
        let pool = init_test_db().await.unwrap();

        for table in ["messages", "presence", "todos"] {
            let col_check = format!("SELECT team_id FROM {} LIMIT 0", table);
            sqlx::query(&col_check)
                .fetch_optional(&pool)
                .await
                .unwrap_or_else(|e| panic!("team_id column missing on {}: {}", table, e));
        }
    }

    #[tokio::test]
    async fn worker_lease_identity_is_unique_per_team() {
        let pool = init_test_db().await.unwrap();

        let team_id = "team-a";
        let now = "2026-04-17T12:00:00Z";

        sqlx::query("INSERT INTO worker_leases (team_id, instance_id, pid, host, acquired_at, last_heartbeat) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(team_id).bind("webdev").bind(1234).bind("hostA").bind(now).bind(now)
            .execute(&pool).await.unwrap();

        let dup = sqlx::query("INSERT INTO worker_leases (team_id, instance_id, pid, host, acquired_at, last_heartbeat) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(team_id).bind("webdev").bind(5678).bind("hostB").bind(now).bind(now)
            .execute(&pool).await;
        assert!(dup.is_err(), "duplicate (team_id, instance_id) lease must be rejected");

        // Different team, same instance name is allowed.
        sqlx::query("INSERT INTO worker_leases (team_id, instance_id, pid, host, acquired_at, last_heartbeat) VALUES (?, ?, ?, ?, ?, ?)")
            .bind("team-b").bind("webdev").bind(9999).bind("hostC").bind(now).bind(now)
            .execute(&pool).await.unwrap();

        // Legacy (NULL team_id) is a namespace of its own and also singleton.
        sqlx::query("INSERT INTO worker_leases (team_id, instance_id, pid, host, acquired_at, last_heartbeat) VALUES (NULL, ?, ?, ?, ?, ?)")
            .bind("solo").bind(1).bind("hostX").bind(now).bind(now)
            .execute(&pool).await.unwrap();
        let legacy_dup = sqlx::query("INSERT INTO worker_leases (team_id, instance_id, pid, host, acquired_at, last_heartbeat) VALUES (NULL, ?, ?, ?, ?, ?)")
            .bind("solo").bind(2).bind("hostY").bind(now).bind(now)
            .execute(&pool).await;
        assert!(legacy_dup.is_err(), "duplicate legacy (NULL team_id) lease must be rejected");
    }
}
