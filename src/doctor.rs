//! `senapati doctor`: checks config, agents, local servers, MCP, skills and alert channels.

use crate::config::AgentKind;
use crate::engine::Engine;
use std::time::Duration;

#[derive(PartialEq)]
enum Level {
    Pass,
    Warn,
    Fail,
}

struct Report {
    fails: usize,
}

impl Report {
    fn line(&mut self, level: Level, what: &str, detail: impl AsRef<str>) {
        let tag = match level {
            Level::Pass => "PASS",
            Level::Warn => "WARN",
            Level::Fail => {
                self.fails += 1;
                "FAIL"
            }
        };
        println!("{tag}  {what:<28} {}", detail.as_ref());
    }
}

/// Returns the number of failed checks.
pub async fn run(engine: &Engine, config_path: &std::path::Path) -> usize {
    let mut r = Report { fails: 0 };
    r.line(Level::Pass, "config", config_path.display().to_string());
    let probe = engine.home.join("tmp");
    match std::fs::create_dir_all(&probe) {
        Ok(()) => r.line(Level::Pass, "home", engine.home.display().to_string()),
        Err(e) => r.line(Level::Fail, "home", format!("{} not writable: {e}", engine.home.display())),
    }

    let http = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().unwrap();
    for a in &engine.cfg.agents {
        let what = format!("agent {}", a.name);
        if !a.enabled {
            r.line(Level::Warn, &what, "disabled");
            continue;
        }
        match a.kind {
            AgentKind::OpenaiCompat => {
                let base = a.base_url.as_deref().unwrap_or_default().trim_end_matches('/');
                let mut req = http.get(format!("{base}/models"));
                if let Some(s) = &a.api_key_secret {
                    match crate::secrets::get(s) {
                        Ok(k) => req = req.bearer_auth(k),
                        Err(e) => {
                            r.line(Level::Fail, &what, e.to_string());
                            continue;
                        }
                    }
                }
                match req.send().await.and_then(|x| x.error_for_status()) {
                    Ok(resp) => {
                        let v: serde_json::Value = resp.json().await.unwrap_or_default();
                        let model = a.model.as_deref().unwrap_or_default();
                        let listed = v["data"].as_array().into_iter().flatten().any(|m| m["id"] == model);
                        if listed {
                            r.line(Level::Pass, &what, format!("{base} serves {model}"));
                        } else {
                            r.line(Level::Warn, &what, format!("{base} is up but does not list {model}"));
                        }
                    }
                    Err(_) => r.line(Level::Warn, &what, format!("{base} not reachable (start LM Studio server: `lms server start`, or Ollama)")),
                }
            }
            _ => {
                let cmd = a.command.clone().unwrap_or_else(|| if a.kind == AgentKind::CodexCli { "codex".into() } else { "claude".into() });
                match crate::proc::resolve(&cmd) {
                    Ok(p) => r.line(Level::Pass, &what, p.display().to_string()),
                    Err(e) => r.line(Level::Fail, &what, e.to_string()),
                }
            }
        }
    }

    for name in engine.cfg.mcp.keys() {
        let what = format!("mcp {name}");
        match tokio::time::timeout(Duration::from_secs(90), engine.mcp().check(name)).await {
            Ok(Ok(n)) => r.line(Level::Pass, &what, format!("{n} tools")),
            Ok(Err(e)) => r.line(Level::Warn, &what, format!("{e:#}")),
            Err(_) => r.line(Level::Warn, &what, "no answer within 90 s"),
        }
    }

    let skills = engine.skills(&engine.cwd());
    let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
    r.line(if skills.is_empty() { Level::Warn } else { Level::Pass }, "skills", format!("{}: {}", skills.len(), names.join(", ")));

    let n = &engine.cfg.notify;
    if n.telegram.enabled {
        let ok = crate::secrets::is_set("telegram_bot_token");
        r.line(if ok { Level::Pass } else { Level::Fail }, "telegram", if ok { "token set" } else { "run: senapati secret set telegram_bot_token" });
        if n.telegram.chat_ids.is_empty() {
            r.line(Level::Warn, "telegram chat_ids", "empty: message the bot, then copy the chat id shown in Senapati's notices");
        }
    }
    if n.discord.enabled {
        let ok = crate::secrets::is_set("discord_webhook_url");
        r.line(if ok { Level::Pass } else { Level::Fail }, "discord", if ok { "webhook set" } else { "run: senapati secret set discord_webhook_url" });
    }
    if n.whatsapp.enabled {
        match crate::proc::resolve(n.whatsapp.node.as_deref().unwrap_or("node")) {
            Ok(p) => r.line(Level::Pass, "whatsapp node", p.display().to_string()),
            Err(e) => r.line(Level::Fail, "whatsapp node", e.to_string()),
        }
        match crate::notify::whatsapp_sidecar_dir(n.whatsapp.sidecar_dir.as_deref()) {
            Ok(d) => r.line(Level::Pass, "whatsapp sidecar", d.display().to_string()),
            Err(e) => r.line(Level::Fail, "whatsapp sidecar", e.to_string()),
        }
        let linked = engine.home.join("whatsapp-auth").join("creds.json").exists();
        r.line(if linked { Level::Pass } else { Level::Fail }, "whatsapp link", if linked { "linked" } else { "run: senapati whatsapp link" });
        if n.whatsapp.to.is_empty() {
            r.line(Level::Fail, "whatsapp to", "add your number to notify.whatsapp.to");
        }
    }
    if n.remote_control {
        r.line(Level::Warn, "remote control", "on: allowed chats can start agents that edit files");
    }
    r.fails
}
