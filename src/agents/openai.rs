//! OpenAI-compatible chat completions (LM Studio, Ollama, OpenRouter...).
//! Stateless: history comes from the store. MCP tools are bridged as function tools.

use super::{fail, one_line, AgentEvent, RunCtx, TurnOutput, TurnRequest};
use crate::config::AgentConfig;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

const MAX_TOOL_ROUNDS: usize = 8;

pub async fn run(
    agent: &AgentConfig,
    ctx: &RunCtx,
    req: TurnRequest,
    tx: UnboundedSender<AgentEvent>,
) -> Result<TurnOutput> {
    let base = agent.base_url.as_deref().expect("validated").trim_end_matches('/');
    let model = agent.model.as_deref().expect("validated");
    let key = match &agent.api_key_secret {
        Some(name) => Some(crate::secrets::get(name)?),
        None => None,
    };

    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    for m in &req.history {
        if m.role == "user" || m.role == "assistant" {
            messages.push(json!({"role": m.role, "content": m.content}));
        }
    }
    messages.push(json!({"role": "user", "content": req.prompt}));

    let tools = ctx.mcp.openai_tools(&agent.mcp).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?;

    for _ in 0..=MAX_TOOL_ROUNDS {
        let mut body = json!({"model": model, "messages": messages});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        let mut r = client.post(format!("{base}/chat/completions")).json(&body);
        if let Some(k) = &key {
            r = r.bearer_auth(k);
        }
        let resp = r.send().await.map_err(|e| fail(format!("{}: {}", agent.name, e.without_url())))?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| e.without_url()).context("bad JSON reply")?;
        if !status.is_success() {
            let msg = v["error"]["message"].as_str().map(String::from).unwrap_or(v.to_string());
            return Err(fail(format!("{} HTTP {}: {msg}", agent.name, status.as_u16())));
        }
        let msg = &v["choices"][0]["message"];
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() {
            let text = strip_think(msg["content"].as_str().unwrap_or_default());
            // Small models often write the call as JSON text instead of tool_calls.
            if let Some((name, args)) = text_tool_call(&text, &tools) {
                let _ = tx.send(AgentEvent::Tool(format!("{name} {}", one_line(&args.to_string(), 120))));
                let result = match ctx.mcp.call_openai_tool(&agent.mcp, &name, args).await {
                    Ok(t) => t,
                    Err(e) => format!("tool error: {e:#}"),
                };
                messages.push(json!({"role": "assistant", "content": text}));
                messages.push(json!({"role": "user", "content": format!(
                    "Tool `{name}` returned:\n{}\n\nUse this result to answer.", super::tail(&result, 20_000))}));
                continue;
            }
            let _ = tx.send(AgentEvent::Text(text.clone()));
            return Ok(TurnOutput { text, vendor_id: None });
        }
        messages.push(msg.clone());
        for call in calls {
            let name = call["function"]["name"].as_str().unwrap_or_default().to_string();
            let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
            let args: Value = serde_json::from_str(raw).unwrap_or(json!({}));
            let _ = tx.send(AgentEvent::Tool(format!("{name} {}", one_line(raw, 120))));
            let result = match ctx.mcp.call_openai_tool(&agent.mcp, &name, args).await {
                Ok(t) => t,
                Err(e) => format!("tool error: {e:#}"),
            };
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call["id"],
                "content": super::tail(&result, 20_000),
            }));
        }
    }
    Err(anyhow::anyhow!("{}: stopped after {MAX_TOOL_ROUNDS} tool rounds", agent.name))
}

/// A reply that is only `{"name": .., "arguments": ..}` (bare, fenced or in <tool_call>)
/// naming a known tool. Names match ignoring case and punctuation.
pub fn text_tool_call(text: &str, tools: &[Value]) -> Option<(String, Value)> {
    let t = text.trim();
    let t = t.strip_prefix("<tool_call>").and_then(|s| s.strip_suffix("</tool_call>")).unwrap_or(t).trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).and_then(|s| s.strip_suffix("```")).unwrap_or(t).trim();
    let v: Value = serde_json::from_str(t).ok()?;
    let norm = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    let wanted = norm(v["name"].as_str()?);
    let name = tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .find(|n| norm(n) == wanted)?
        .to_string();
    let args = match &v["arguments"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
        Value::Null => json!({}),
        other => other.clone(),
    };
    Some((name, args))
}

/// Reasoning models wrap their scratchpad in <think>...</think>; keep only the answer.
pub fn strip_think(s: &str) -> String {
    match s.rfind("</think>") {
        Some(i) => s[i + "</think>".len()..].trim().to_string(),
        None => s.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn text_tool_calls() {
        let tools = vec![json!({"type": "function", "function": {"name": "caveman__caveman_toon_encode"}})];
        let (n, a) = super::text_tool_call(r#"{"name": "caveman_caveman_toon_encode", "arguments": {"input": "x"}}"#, &tools).unwrap();
        assert_eq!(n, "caveman__caveman_toon_encode");
        assert_eq!(a["input"], "x");
        let fenced = "```json\n{\"name\":\"caveman__caveman_toon_encode\",\"arguments\":\"{\\\"input\\\":1}\"}\n```";
        assert_eq!(super::text_tool_call(fenced, &tools).unwrap().1["input"], 1);
        assert!(super::text_tool_call(r#"{"name": "other", "arguments": {}}"#, &tools).is_none());
        assert!(super::text_tool_call("plain answer", &tools).is_none());
    }

    #[test]
    fn strips_think() {
        assert_eq!(super::strip_think("<think>hmm</think>\nAnswer"), "Answer");
        assert_eq!(super::strip_think("plain"), "plain");
    }
}
