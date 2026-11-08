//! Claude Code headless: `claude -p --output-format stream-json --verbose`, prompt on stdin.

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
    let mut args: Vec<String> =
        ["-p", "--output-format", "stream-json", "--verbose"].map(String::from).to_vec();
    if !req.system.is_empty() {
        args.push("--append-system-prompt".into());
        args.push(req.system.clone());
    }
    if let Some(m) = &agent.model {
        args.extend(["--model".into(), m.clone()]);
    }
    if let Some(p) = &agent.permission_mode {
        args.extend(["--permission-mode".into(), p.clone()]);
    }
    if let Some(id) = &req.resume {
        args.extend(["--resume".into(), id.clone()]);
    }
    if !agent.mcp.is_empty() {
        let path = ctx.home.join("tmp").join(format!("mcp-{}.json", agent.name));
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, ctx.mcp.claude_config_json(&agent.mcp)?)?;
        args.extend(["--mcp-config".into(), path.to_string_lossy().to_string()]);
    }
    args.extend(agent.args.iter().cloned());

    let mut cmd = crate::proc::command(agent.command.as_deref().unwrap_or("claude"), &args)?;
    cmd.current_dir(&req.cwd);

    let mut out = TurnOutput::default();
    let mut result: Option<(bool, String)> = None;
    let (ok, stderr) = run_lines(cmd, &req.prompt, |line| {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if let Some(id) = v["session_id"].as_str() {
            if out.vendor_id.as_deref() != Some(id) {
                out.vendor_id = Some(id.to_string());
                let _ = tx.send(AgentEvent::VendorId(id.to_string()));
            }
        }
        match v["type"].as_str() {
            Some("assistant") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let t = block["text"].as_str().unwrap_or_default().to_string();
                            if !t.trim().is_empty() {
                                out.text = t.clone();
                                let _ = tx.send(AgentEvent::Text(t));
                            }
                        }
                        Some("tool_use") => {
                            let name = block["name"].as_str().unwrap_or("tool");
                            let input = one_line(&block["input"].to_string(), 120);
                            let _ = tx.send(AgentEvent::Tool(format!("{name} {input}")));
                        }
                        _ => {}
                    }
                }
            }
            Some("result") => {
                let is_err = v["is_error"].as_bool().unwrap_or(false)
                    || v["subtype"].as_str().is_some_and(|s| s != "success");
                let text = v["result"].as_str().unwrap_or_default().to_string();
                result = Some((is_err, text));
            }
            _ => {}
        }
    })
    .await?;

    match result {
        Some((false, text)) => {
            if !text.trim().is_empty() {
                out.text = text;
            }
            Ok(out)
        }
        Some((true, text)) => Err(fail(format!("claude: {text} {stderr}"))),
        None if ok => Ok(out),
        None => Err(fail(format!("claude exited without a result: {stderr}"))),
    }
}
