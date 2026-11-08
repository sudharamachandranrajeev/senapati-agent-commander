//! Codex headless: `codex exec --json [...] -` (or `... resume <thread> -`), prompt on stdin.

use super::{fail, one_line, run_lines, AgentEvent, RunCtx, TurnOutput, TurnRequest};
use crate::config::AgentConfig;
use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

pub async fn run(
    agent: &AgentConfig,
    ctx: &RunCtx,
    req: TurnRequest,
    tx: UnboundedSender<AgentEvent>,
) -> Result<TurnOutput> {
    let mut args: Vec<String> = vec![
        "exec".into(),
        "--json".into(),
        "--skip-git-repo-check".into(),
        "-C".into(),
        req.cwd.to_string_lossy().to_string(),
    ];
    if let Some(s) = &agent.sandbox {
        args.extend(["-s".into(), s.clone()]);
    }
    if let Some(m) = &agent.model {
        args.extend(["-m".into(), m.clone()]);
    }
    for kv in ctx.mcp.codex_overrides(&agent.mcp)? {
        args.extend(["-c".into(), kv]);
    }
    args.extend(agent.args.iter().cloned());
    if let Some(id) = &req.resume {
        args.extend(["resume".into(), id.clone()]);
    }
    args.push("-".into());

    let mut cmd = crate::proc::command(agent.command.as_deref().unwrap_or("codex"), &args)?;
    cmd.current_dir(&req.cwd);

    let input = if req.system.is_empty() {
        req.prompt.clone()
    } else {
        format!("<instructions>\n{}\n</instructions>\n\n{}", req.system, req.prompt)
    };

    let mut out = TurnOutput::default();
    let mut error: Option<String> = None;
    let (ok, stderr) = run_lines(cmd, &input, |line| {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        match v["type"].as_str() {
            Some("thread.started") => {
                if let Some(id) = v["thread_id"].as_str() {
                    out.vendor_id = Some(id.to_string());
                    let _ = tx.send(AgentEvent::VendorId(id.to_string()));
                }
            }
            Some("item.started") | Some("item.completed") => {
                let item = &v["item"];
                let done = v["type"] == "item.completed";
                match item["type"].as_str() {
                    Some("agent_message") if done => {
                        let t = item["text"].as_str().unwrap_or_default().to_string();
                        if !t.trim().is_empty() {
                            out.text = t.clone();
                            let _ = tx.send(AgentEvent::Text(t));
                        }
                    }
                    Some("command_execution") if !done => {
                        let c = item["command"].as_str().unwrap_or_default();
                        let _ = tx.send(AgentEvent::Tool(format!("shell {}", one_line(c, 120))));
                    }
                    Some("file_change") if done => {
                        let files: Vec<&str> = item["changes"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|c| c["path"].as_str())
                            .collect();
                        let _ = tx.send(AgentEvent::Tool(format!("edit {}", files.join(", "))));
                    }
                    Some("mcp_tool_call") if !done => {
                        let s = item["server"].as_str().unwrap_or_default();
                        let t = item["tool"].as_str().unwrap_or_default();
                        let _ = tx.send(AgentEvent::Tool(format!("mcp {s}.{t}")));
                    }
                    _ => {}
                }
            }
            Some("turn.failed") => {
                error = Some(v["error"]["message"].as_str().unwrap_or("turn failed").to_string());
            }
            Some("error") => {
                error = Some(v["message"].as_str().unwrap_or("error").to_string());
            }
            _ => {}
        }
    })
    .await?;

    if let Some(e) = error {
        return Err(fail(format!("codex: {e}")));
    }
    if !ok {
        return Err(fail(format!("codex exited with an error: {stderr}")));
    }
    Ok(out)
}
