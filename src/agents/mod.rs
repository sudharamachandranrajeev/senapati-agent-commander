//! Provider adapters. Each runs one turn and streams activity back.

mod claude;
mod cli;
mod codex;
mod openai;

use crate::config::{AgentConfig, AgentKind};
use crate::mcp::McpHub;
use crate::store::Message;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

pub struct TurnRequest {
    pub prompt: String,
    /// Extra system instructions: caveman layer, active skill, plan role.
    pub system: String,
    pub cwd: PathBuf,
    /// Vendor-side session to resume (claude session id, codex thread id).
    pub resume: Option<String>,
    /// Earlier messages, for stateless providers.
    pub history: Vec<Message>,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    VendorId(String),
    Text(String),
    Tool(String),
}

#[derive(Debug, Default)]
pub struct TurnOutput {
    pub text: String,
    pub vendor_id: Option<String>,
}

/// The provider refused because a usage or rate limit was hit. Triggers failover.
#[derive(Debug)]
pub struct LimitHit(pub String);

impl std::fmt::Display for LimitHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "limit hit: {}", self.0)
    }
}
impl std::error::Error for LimitHit {}

pub fn looks_like_limit(text: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)usage limit|rate[ _-]?limit|limit reached|hit your limit|quota|too many requests|\b429\b|credit balance",
        )
        .unwrap()
    })
    .is_match(text)
}

/// Turn a failed run's message into LimitHit or a plain error.
pub fn fail(msg: String) -> anyhow::Error {
    if looks_like_limit(&msg) {
        LimitHit(msg).into()
    } else {
        anyhow::anyhow!(msg)
    }
}

pub struct RunCtx {
    pub home: PathBuf,
    pub mcp: Arc<McpHub>,
}

pub async fn run(
    agent: &AgentConfig,
    ctx: &RunCtx,
    req: TurnRequest,
    tx: UnboundedSender<AgentEvent>,
) -> Result<TurnOutput> {
    match agent.kind {
        AgentKind::ClaudeCli => claude::run(agent, ctx, req, tx).await,
        AgentKind::CodexCli => codex::run(agent, ctx, req, tx).await,
        AgentKind::OpenaiCompat => openai::run(agent, ctx, req, tx).await,
        AgentKind::Cli => cli::run(agent, req, tx).await,
    }
}

/// Spawn a CLI, write `stdin_text`, call `on_line` per stdout line.
/// Returns (exit success, stderr tail).
async fn run_lines(
    mut cmd: tokio::process::Command,
    stdin_text: &str,
    mut on_line: impl FnMut(&str),
) -> Result<(bool, String)> {
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = stdin_text.to_string();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(input.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let mut stderr = child.stderr.take().expect("piped");
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    while let Some(line) = lines.next_line().await? {
        on_line(&line);
    }
    let status = child.wait().await?;
    let _ = writer.await;
    let err = err_task.await.unwrap_or_default();
    Ok((status.success(), tail(&err, 1500)))
}

pub fn tail(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

pub fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_detection() {
        assert!(looks_like_limit("Claude AI usage limit reached|1731000000"));
        assert!(looks_like_limit("HTTP 429 Too Many Requests"));
        assert!(looks_like_limit("You've hit your limit"));
        assert!(!looks_like_limit("file not found"));
        assert!(fail("rate_limit_error".into()).downcast_ref::<LimitHit>().is_some());
        assert!(fail("boom".into()).downcast_ref::<LimitHit>().is_none());
    }

    #[test]
    fn tail_respects_char_boundaries() {
        let s = "ééééé";
        let t = tail(s, 3);
        assert!(t.starts_with('…'));
    }
}
