//! Alert channels: Telegram (two-way), Discord webhook (one-way), WhatsApp linked account (two-way).
//! Secrets never appear in errors: reqwest errors are stripped of their URL.

mod whatsapp;

use crate::engine::Engine;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Weak};
use std::time::Duration;

pub use whatsapp::{link as whatsapp_link, sidecar_dir as whatsapp_sidecar_dir};

pub struct Notifier {
    http: reqwest::Client,
    telegram: Option<Telegram>,
    discord: Option<String>,
    whatsapp: Option<whatsapp::WhatsApp>,
    engine: Weak<Engine>,
}

struct Telegram {
    token: String,
    chat_ids: Vec<i64>,
}

impl Notifier {
    /// Build enabled channels. A channel with a missing secret is skipped with a notice.
    /// `listen` starts the inbound side (Telegram polling, WhatsApp messages).
    pub async fn start(engine: &Arc<Engine>, listen: bool) -> Arc<Self> {
        let cfg = &engine.cfg.notify;
        let http = reqwest::Client::builder().timeout(Duration::from_secs(70)).build().expect("http client");
        let telegram = if cfg.telegram.enabled {
            match crate::secrets::get("telegram_bot_token") {
                Ok(token) => Some(Telegram { token, chat_ids: cfg.telegram.chat_ids.clone() }),
                Err(e) => {
                    engine.notice(format!("telegram off: {e}"));
                    None
                }
            }
        } else {
            None
        };
        let discord = if cfg.discord.enabled {
            match crate::secrets::get("discord_webhook_url") {
                Ok(u) if u.starts_with("https://discord.com/api/webhooks/") || u.starts_with("https://discordapp.com/api/webhooks/") => Some(u),
                Ok(_) => {
                    engine.notice("discord off: discord_webhook_url is not a Discord webhook URL");
                    None
                }
                Err(e) => {
                    engine.notice(format!("discord off: {e}"));
                    None
                }
            }
        } else {
            None
        };
        let whatsapp = if cfg.whatsapp.enabled {
            match whatsapp::WhatsApp::start(engine, listen).await {
                Ok(w) => Some(w),
                Err(e) => {
                    engine.notice(format!("whatsapp off: {e:#}"));
                    None
                }
            }
        } else {
            None
        };
        let n = Arc::new(Self { http, telegram, discord, whatsapp, engine: Arc::downgrade(engine) });
        if listen && n.telegram.is_some() {
            let n2 = n.clone();
            tokio::spawn(async move { n2.poll_telegram().await });
        }
        n
    }

    pub fn channels(&self) -> Vec<&'static str> {
        let mut v = vec![];
        if self.telegram.is_some() {
            v.push("telegram");
        }
        if self.discord.is_some() {
            v.push("discord");
        }
        if self.whatsapp.is_some() {
            v.push("whatsapp");
        }
        v
    }

    fn notice(&self, s: String) {
        if let Some(e) = self.engine.upgrade() {
            e.notice(s);
        }
    }

    pub async fn send(&self, text: &str) {
        let text = format!("[senapati] {text}");
        if let Some(t) = &self.telegram {
            for id in &t.chat_ids {
                if let Err(e) = self.telegram_send(t, *id, &text).await {
                    self.notice(format!("telegram send failed: {e}"));
                }
            }
        }
        if let Some(url) = &self.discord {
            for chunk in chunks(&text, 1900) {
                let r = self.http.post(url).json(&json!({"content": chunk})).send().await;
                if let Err(e) = r.map_err(|e| e.without_url()).and_then(|r| r.error_for_status().map_err(|e| e.without_url())) {
                    self.notice(format!("discord send failed: {e}"));
                }
            }
        }
        if let Some(w) = &self.whatsapp {
            if let Err(e) = w.send_all(&text).await {
                self.notice(format!("whatsapp send failed: {e}"));
            }
        }
    }

    async fn telegram_send(&self, t: &Telegram, chat: i64, text: &str) -> Result<()> {
        for chunk in chunks(text, 4000) {
            let r = self
                .http
                .post(format!("https://api.telegram.org/bot{}/sendMessage", t.token))
                .json(&json!({"chat_id": chat, "text": chunk}))
                .send()
                .await
                .map_err(|e| e.without_url())?;
            if !r.status().is_success() {
                return Err(anyhow!("HTTP {}", r.status().as_u16()));
            }
        }
        Ok(())
    }

    async fn poll_telegram(self: Arc<Self>) {
        let Some(t) = &self.telegram else { return };
        let mut offset: i64 = 0;
        loop {
            let r = self
                .http
                .get(format!("https://api.telegram.org/bot{}/getUpdates", t.token))
                .query(&[("timeout", "50"), ("offset", &offset.to_string()), ("allowed_updates", "[\"message\"]")])
                .send()
                .await
                .map_err(|e| e.without_url());
            let v: Value = match r {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(_) => Value::Null,
                },
                Err(e) => {
                    self.notice(format!("telegram poll: {e}"));
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };
            if v["ok"] != true {
                // 409: another senapati instance is polling the same bot.
                let desc = v["description"].as_str().unwrap_or("bad reply").to_string();
                self.notice(format!("telegram poll: {desc}"));
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            for u in v["result"].as_array().into_iter().flatten() {
                offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
                let chat = u["message"]["chat"]["id"].as_i64().unwrap_or(0);
                let Some(text) = u["message"]["text"].as_str() else { continue };
                if !t.chat_ids.contains(&chat) {
                    self.notice(format!("telegram: ignored message from chat {chat} (not in notify.telegram.chat_ids)"));
                    continue;
                }
                let Some(engine) = self.engine.upgrade() else { return };
                let reply = crate::remote::handle(&engine, text).await;
                if let Err(e) = self.telegram_send(t, chat, &reply).await {
                    self.notice(format!("telegram reply failed: {e}"));
                }
            }
        }
    }
}

pub fn chunks(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if cur.len() + ch.len_utf8() > max {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn chunking() {
        let c = super::chunks(&"é".repeat(10), 4);
        assert_eq!(c.len(), 5);
        assert!(c.iter().all(|x| x.len() <= 4));
    }
}
