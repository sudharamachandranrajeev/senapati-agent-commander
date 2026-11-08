//! WhatsApp through a linked account (Baileys sidecar in sidecars/whatsapp).
//! Unofficial client: WhatsApp may ban the linked number. Use a spare number.
//! Protocol: JSON lines. To sidecar: {"op":"send","to":"<digits>","text":".."}.
//! From sidecar: {"event":"ready"|"qr"|"closed"|"logged_out"|"error"|"message", ...}.

use crate::engine::Engine;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex;

pub struct WhatsApp {
    stdin: Arc<Mutex<ChildStdin>>,
    to: Vec<String>,
    _child: Child,
}

pub fn auth_dir(home: &Path) -> PathBuf {
    home.join("whatsapp-auth")
}

pub fn sidecar_dir(configured: Option<&str>) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(c) = configured {
        candidates.push(PathBuf::from(c));
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        candidates.push(exe_dir.join("sidecars").join("whatsapp"));
        // cargo builds land in <repo>\target\<profile>
        candidates.push(exe_dir.join("..").join("..").join("sidecars").join("whatsapp"));
    }
    candidates.push(PathBuf::from("sidecars").join("whatsapp"));
    for c in candidates {
        if c.join("index.mjs").exists() {
            if !c.join("node_modules").exists() {
                bail!("run `npm ci` in {} first", c.display());
            }
            return Ok(c);
        }
    }
    bail!("WhatsApp sidecar not found; set notify.whatsapp.sidecar_dir")
}

/// Foreground QR linking: prints the QR in this terminal, exits once linked.
pub fn link(engine: &Engine) -> Result<()> {
    let cfg = &engine.cfg.notify.whatsapp;
    let dir = sidecar_dir(cfg.sidecar_dir.as_deref())?;
    let node = crate::proc::resolve(cfg.node.as_deref().unwrap_or("node"))?;
    let auth = auth_dir(&engine.home);
    std::fs::create_dir_all(&auth)?;
    let status = std::process::Command::new(node)
        .arg(dir.join("index.mjs"))
        .arg("--auth")
        .arg(&auth)
        .arg("--link")
        .status()
        .context("start node")?;
    if !status.success() {
        bail!("linking failed");
    }
    Ok(())
}

impl WhatsApp {
    pub async fn start(engine: &Arc<Engine>, listen: bool) -> Result<Self> {
        let cfg = &engine.cfg.notify.whatsapp;
        let auth = auth_dir(&engine.home);
        if !auth.join("creds.json").exists() {
            bail!("not linked; run `senapati whatsapp link`");
        }
        let dir = sidecar_dir(cfg.sidecar_dir.as_deref())?;
        let args = vec![dir.join("index.mjs").to_string_lossy().to_string(), "--auth".into(), auth.to_string_lossy().to_string()];
        let mut cmd = crate::proc::command(cfg.node.as_deref().unwrap_or("node"), &args)?;
        cmd.stderr(std::process::Stdio::null());
        let mut child = cmd.spawn()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped")));
        let stdout = child.stdout.take().expect("piped");
        let (weak, w, allowed) = (Arc::downgrade(engine), stdin.clone(), cfg.to.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let Some(engine) = weak.upgrade() else { return };
                match v["event"].as_str() {
                    Some("ready") => engine.notice("whatsapp connected"),
                    Some("qr") => engine.notice("whatsapp needs relinking: run `senapati whatsapp link`"),
                    Some("logged_out") => engine.notice("whatsapp logged out: run `senapati whatsapp link`"),
                    Some("error") => engine.notice(format!("whatsapp: {}", v["message"].as_str().unwrap_or("error"))),
                    Some("message") if listen => {
                        let from = v["from"].as_str().unwrap_or_default().to_string();
                        let text = v["text"].as_str().unwrap_or_default().to_string();
                        if !allowed.contains(&from) {
                            continue;
                        }
                        let reply = crate::remote::handle(&engine, &text).await;
                        let _ = send_line(&w, &from, &format!("[senapati] {reply}")).await;
                    }
                    _ => {}
                }
            }
            if let Some(e) = weak.upgrade() {
                e.notice("whatsapp sidecar stopped");
            }
        });
        Ok(Self { stdin, to: cfg.to.clone(), _child: child })
    }

    pub async fn send_all(&self, text: &str) -> Result<()> {
        for n in &self.to {
            send_line(&self.stdin, n, text).await?;
        }
        Ok(())
    }
}

async fn send_line(w: &Mutex<ChildStdin>, to: &str, text: &str) -> Result<()> {
    let mut line = json!({"op": "send", "to": to, "text": text}).to_string();
    line.push('\n');
    let mut w = w.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}
