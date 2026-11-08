//! Any agent CLI that reads a prompt on stdin and prints plain text (e.g. gemini -p).
//! Stateless: earlier turns are replayed in the prompt.

use super::{fail, run_lines, AgentEvent, TurnOutput, TurnRequest};
use crate::config::AgentConfig;
use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

pub async fn run(
    agent: &AgentConfig,
    req: TurnRequest,
    tx: UnboundedSender<AgentEvent>,
) -> Result<TurnOutput> {
    let command = agent.command.as_deref().expect("validated");
    let mut cmd = crate::proc::command(command, &agent.args)?;
    cmd.current_dir(&req.cwd);
    let input = transcript_prompt(&req);
    let mut text = String::new();
    let (ok, stderr) = run_lines(cmd, &input, |line| {
        text.push_str(line);
        text.push('\n');
    })
    .await?;
    if !ok {
        return Err(fail(format!("{}: {stderr}", agent.name)));
    }
    let text = text.trim().to_string();
    let _ = tx.send(AgentEvent::Text(text.clone()));
    Ok(TurnOutput { text, vendor_id: None })
}

pub fn transcript_prompt(req: &TurnRequest) -> String {
    let mut s = String::new();
    if !req.system.is_empty() {
        s.push_str(&format!("<instructions>\n{}\n</instructions>\n\n", req.system));
    }
    let turns: Vec<_> =
        req.history.iter().filter(|m| m.role == "user" || m.role == "assistant").collect();
    if !turns.is_empty() {
        s.push_str("<conversation>\n");
        for m in turns {
            s.push_str(&format!("[{}]\n{}\n\n", m.role, m.content));
        }
        s.push_str("</conversation>\n\n");
    }
    s.push_str(&req.prompt);
    s
}
