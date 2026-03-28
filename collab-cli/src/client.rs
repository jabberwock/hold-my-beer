use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub hash: String,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub refs: Vec<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub instance_id: String,
    pub role: String,
    pub last_seen: DateTime<Utc>,
    pub message_count: usize,
}

pub struct CollabClient {
    base_url: String,
    instance_id: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl CollabClient {
    pub fn new(base_url: &str, instance_id: &str, token: Option<&str>) -> Self {
        Self {
            base_url: base_url.to_string(),
            instance_id: instance_id.to_string(),
            token: token.map(|t| t.to_string()),
            client: reqwest::Client::new(),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            req.header("Authorization", format!("Bearer {}", token))
        } else {
            req
        }
    }

    pub async fn heartbeat(&self, role: Option<&str>) -> Result<()> {
        #[derive(Serialize)]
        struct PresenceUpdate {
            role: Option<String>,
        }

        let url = format!("{}/presence/{}", self.base_url, self.instance_id);
        self.auth(self.client.put(&url))
            .json(&PresenceUpdate { role: role.map(|r| r.to_string()) })
            .send()
            .await?;
        Ok(())
    }

    pub async fn list_messages(&self) -> Result<()> {
        let url = format!("{}/messages/{}", self.base_url, self.instance_id);

        let response = self.auth(self.client.get(&url)).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch messages: {}", response.status());
        }

        let messages: Vec<Message> = response.json().await?;

        if messages.is_empty() {
            println!("No messages in the last hour.");
            return Ok(());
        }

        println!("Messages for @{}:\n", self.instance_id);
        for msg in messages {
            println!("─────────────────────────────────────");
            println!("Hash: {}", &msg.hash[..7]);
            println!("From: @{}", msg.sender);
            println!("Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
            if !msg.refs.is_empty() {
                let short_refs: Vec<String> = msg.refs.iter()
                    .map(|r| r.chars().take(7).collect())
                    .collect();
                println!("Refs: {}", short_refs.join(", "));
            }
            println!("\n{}\n", msg.content);
        }
        println!("─────────────────────────────────────");

        Ok(())
    }

    pub async fn add_message(
        &self,
        recipient: &str,
        content: &str,
        refs: Option<Vec<String>>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct CreateMessage {
            sender: String,
            recipient: String,
            content: String,
            refs: Vec<String>,
        }

        let payload = CreateMessage {
            sender: self.instance_id.clone(),
            recipient: recipient.to_string(),
            content: content.to_string(),
            refs: refs.unwrap_or_default(),
        };

        let url = format!("{}/messages", self.base_url);

        let response = self.auth(self.client.post(&url))
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to send message: {}", response.status());
        }

        let msg: Message = response.json().await?;

        println!("✓ Message sent to @{}", recipient);
        println!("  Hash: {}", &msg.hash[..7]);
        println!("  Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));

        Ok(())
    }

    pub async fn watch_messages(
        &self,
        interval_secs: u64,
        role: Option<String>,
        recipients: Vec<String>,
    ) -> Result<()> {
        use tokio::time::{sleep, Duration};

        let mut seen_ids: HashSet<String> = HashSet::new();
        // Track which recipients we've already warned about
        let mut warned_missing: HashSet<String> = HashSet::new();
        // Track which recipients have been seen at least once
        let mut seen_recipients: HashSet<String> = HashSet::new();

        let role_str = role.as_deref();

        println!("Watching for messages to @{} (polling every {}s)", self.instance_id, interval_secs);
        if !recipients.is_empty() {
            println!("Waiting for: {}", recipients.iter().map(|r| format!("@{}", r)).collect::<Vec<_>>().join(", "));
        }
        println!("Press Ctrl+C to stop\n");

        loop {
            // Heartbeat presence
            if let Err(e) = self.heartbeat(role_str).await {
                eprintln!("Warning: presence heartbeat failed: {}", e);
            }

            // Check roster for configured recipients
            if !recipients.is_empty() {
                if let Ok(roster) = self.fetch_roster().await {
                    let online: HashSet<String> = roster.iter().map(|w| w.instance_id.clone()).collect();

                    for recipient in &recipients {
                        let r = recipient.trim_start_matches('@').to_string();
                        if online.contains(&r) {
                            if !seen_recipients.contains(&r) {
                                seen_recipients.insert(r.clone());
                                warned_missing.remove(&r);
                                println!("── @{} is online ──", r);
                            }
                        } else if !warned_missing.contains(&r) {
                            warned_missing.insert(r.clone());
                            // Only warn after they were previously seen (went offline)
                            if seen_recipients.contains(&r) {
                                println!("── @{} went offline ──", r);
                            }
                        }
                    }

                    // On first poll, report any recipients not yet online
                    if seen_ids.is_empty() {
                        let missing: Vec<_> = recipients.iter()
                            .filter(|r| {
                                let r = r.trim_start_matches('@');
                                !online.contains(r)
                            })
                            .map(|r| format!("@{}", r.trim_start_matches('@')))
                            .collect();
                        if !missing.is_empty() {
                            println!("Not yet online: {}", missing.join(", "));
                        }
                    }
                }
            }

            // Poll for new messages
            let url = format!("{}/messages/{}", self.base_url, self.instance_id);
            match self.auth(self.client.get(&url)).send().await {
                Ok(response) if response.status().is_success() => {
                    match response.json::<Vec<Message>>().await {
                        Ok(messages) => {
                            let new_messages: Vec<_> = messages
                                .into_iter()
                                .filter(|msg| !seen_ids.contains(&msg.id))
                                .collect();

                            for msg in &new_messages {
                                seen_ids.insert(msg.id.clone());

                                println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                                println!("New message from @{}", msg.sender);
                                println!("Hash: {}  Time: {}", &msg.hash[..7], msg.timestamp.format("%H:%M:%S UTC"));
                                if !msg.refs.is_empty() {
                                    let short_refs: Vec<String> = msg.refs.iter()
                                        .map(|r| r.chars().take(7).collect())
                                        .collect();
                                    println!("Refs: {}", short_refs.join(", "));
                                }
                                println!("\n{}\n", msg.content);
                                println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
                            }
                        }
                        Err(e) => eprintln!("Warning: failed to parse messages: {}", e),
                    }
                }
                Ok(response) => eprintln!("Warning: server error: {}", response.status()),
                Err(e) => eprintln!("Warning: connection error: {}", e),
            }

            sleep(Duration::from_secs(interval_secs)).await;
        }
    }

    async fn fetch_roster(&self) -> Result<Vec<WorkerInfo>> {
        self.fetch_roster_pub().await
    }

    pub async fn fetch_roster_pub(&self) -> Result<Vec<WorkerInfo>> {
        let url = format!("{}/roster", self.base_url);
        let response = self.auth(self.client.get(&url)).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("Server error: {}", response.status());
        }
        Ok(response.json::<Vec<WorkerInfo>>().await?)
    }

    pub async fn fetch_history_pub(&self, instance_id: &str) -> Result<Vec<Message>> {
        let url = format!("{}/history/{}", self.base_url, instance_id);
        let response = self.auth(self.client.get(&url)).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("Server error: {}", response.status());
        }
        Ok(response.json::<Vec<Message>>().await?)
    }

    pub async fn show_history(&self, filter_instance: Option<&str>) -> Result<()> {
        let url = format!("{}/history/{}", self.base_url, self.instance_id);

        let response = self.auth(self.client.get(&url)).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch history: {}", response.status());
        }

        let mut messages: Vec<Message> = response.json().await?;

        if let Some(filter_id) = filter_instance {
            messages.retain(|msg| msg.sender == filter_id || msg.recipient == filter_id);
        }

        if messages.is_empty() {
            println!("No message history in the last hour.");
            if let Some(filter_id) = filter_instance {
                println!("(filtered to conversations with @{})", filter_id);
            }
            return Ok(());
        }

        println!("Message History for @{}:\n", self.instance_id);
        if let Some(filter_id) = filter_instance {
            println!("(showing only conversations with @{})\n", filter_id);
        }

        for msg in messages {
            let direction = if msg.sender == self.instance_id {
                format!("→ @{}", msg.recipient)
            } else {
                format!("← @{}", msg.sender)
            };

            println!("─────────────────────────────────────");
            println!("{} [{}]", direction, &msg.hash[..7]);
            println!("Time: {}", msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
            if !msg.refs.is_empty() {
                let short_refs: Vec<String> = msg.refs.iter()
                    .map(|r| r.chars().take(7).collect())
                    .collect();
                println!("Refs: {}", short_refs.join(", "));
            }
            println!("\n{}\n", msg.content);
        }
        println!("─────────────────────────────────────");

        Ok(())
    }

    pub async fn show_roster(&self) -> Result<()> {
        let workers = self.fetch_roster().await?;

        if workers.is_empty() {
            println!("No active workers.");
            return Ok(());
        }

        println!("Active workers:\n");
        for worker in workers {
            let you = if worker.instance_id == self.instance_id { " (you)" } else { "" };
            print!("  @{}{}", worker.instance_id, you);
            if !worker.role.is_empty() {
                print!("  —  {}", worker.role);
            }
            println!();
            println!("    Last seen: {}", worker.last_seen.format("%H:%M:%S UTC"));
            if worker.message_count > 0 {
                println!("    Messages: {}", worker.message_count);
            }
            println!();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization() {
        let message = Message {
            id: "test-id".to_string(),
            hash: "abc123".to_string(),
            sender: "worker1".to_string(),
            recipient: "worker2".to_string(),
            content: "test content".to_string(),
            refs: vec!["ref1".to_string()],
            timestamp: Utc::now(),
        };

        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("test-id"));
        assert!(json.contains("worker1"));
    }

    #[test]
    fn test_message_deserialization() {
        let json = r#"{
            "id": "test-id",
            "hash": "abc123",
            "sender": "worker1",
            "recipient": "worker2",
            "content": "test content",
            "refs": ["ref1"],
            "timestamp": "2024-03-27T14:30:45Z"
        }"#;

        let message: Message = serde_json::from_str(json).unwrap();
        assert_eq!(message.id, "test-id");
        assert_eq!(message.sender, "worker1");
        assert_eq!(message.refs.len(), 1);
    }

    #[test]
    fn test_collab_client_creation() {
        let client = CollabClient::new("http://localhost:8000", "test-worker", None);
        assert_eq!(client.base_url, "http://localhost:8000");
        assert_eq!(client.instance_id, "test-worker");
    }
}
