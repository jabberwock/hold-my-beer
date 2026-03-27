use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

pub async fn init_db() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite:collab.db")?
        .create_if_missing(true);

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
            timestamp TEXT NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_recipient_timestamp
        ON messages(recipient, timestamp DESC)
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS presence (
            instance_id TEXT PRIMARY KEY,
            role TEXT NOT NULL DEFAULT '',
            last_seen TEXT NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await?;

    Ok(pool)
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
            timestamp TEXT NOT NULL
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

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS presence (
            instance_id TEXT PRIMARY KEY,
            role TEXT NOT NULL DEFAULT '',
            last_seen TEXT NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await?;

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
}
