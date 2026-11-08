//! MCP servers shared by every provider.
//! Claude gets them via --mcp-config, codex via -c overrides,
//! local models through this minimal stdio JSON-RPC client.

use crate::config::McpServerConfig;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

const CALL_TIMEOUT: Duration = Duration::from_secs(300);
const START_TIMEOUT: Duration = Duration::from_secs(120);

pub struct McpHub {
    servers: BTreeMap<String, McpServerConfig>,
    clients: Mutex<HashMap<String, Arc<McpClient>>>,
}

#[derive(Clone)]
struct Tool {
    server: String,
    name: String,
    openai_name: String,
    description: String,
    schema: Value,
}

impl McpHub {
    pub fn new(servers: BTreeMap<String, McpServerConfig>) -> Self {
        Self { servers, clients: Mutex::new(HashMap::new()) }
    }

    fn cfg(&self, name: &str) -> Result<&McpServerConfig> {
        self.servers.get(name).ok_or_else(|| anyhow!("unknown mcp server '{name}'"))
    }

    /// `{"mcpServers": {...}}` for `claude --mcp-config`.
    pub fn claude_config_json(&self, names: &[String]) -> Result<String> {
        let mut servers = serde_json::Map::new();
        for n in names {
            let c = self.cfg(n)?;
            let (cmd, args) = crate::proc::launch_spec(&c.command, &c.args)?;
            servers.insert(n.clone(), json!({"command": cmd, "args": args, "env": c.env}));
        }
        Ok(json!({ "mcpServers": servers }).to_string())
    }

    /// `-c mcp_servers.<name>.command=...` overrides for `codex exec`.
    pub fn codex_overrides(&self, names: &[String]) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for n in names {
            let c = self.cfg(n)?;
            let (cmd, args) = crate::proc::launch_spec(&c.command, &c.args)?;
            let args = toml::Value::Array(args.into_iter().map(toml::Value::String).collect());
            out.push(format!("mcp_servers.{n}.command={}", toml::Value::String(cmd)));
            out.push(format!("mcp_servers.{n}.args={args}"));
            for (k, v) in &c.env {
                out.push(format!("mcp_servers.{n}.env.{k}={}", toml::Value::String(v.clone())));
            }
        }
        Ok(out)
    }

    async fn client(&self, name: &str) -> Result<Arc<McpClient>> {
        let mut map = self.clients.lock().await;
        if let Some(c) = map.get(name) {
            if c.alive().await {
                return Ok(c.clone());
            }
        }
        let c = Arc::new(McpClient::start(self.cfg(name)?).await.with_context(|| format!("start mcp '{name}'"))?);
        map.insert(name.to_string(), c.clone());
        Ok(c)
    }

    async fn tools(&self, names: &[String]) -> Result<Vec<Tool>> {
        let mut out = Vec::new();
        for n in names {
            let allow = &self.cfg(n)?.tools;
            let c = self.client(n).await?;
            for t in c.tools().await? {
                let tool = t["name"].as_str().unwrap_or_default().to_string();
                if !allow.is_empty() && !allow.contains(&tool) {
                    continue;
                }
                out.push(Tool {
                    server: n.clone(),
                    openai_name: openai_name(n, &tool),
                    name: tool,
                    description: t["description"].as_str().unwrap_or_default().chars().take(1000).collect(),
                    schema: t.get("inputSchema").cloned().unwrap_or(json!({"type": "object"})),
                });
            }
        }
        Ok(out)
    }

    pub async fn openai_tools(&self, names: &[String]) -> Result<Vec<Value>> {
        Ok(self
            .tools(names)
            .await?
            .into_iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t.openai_name, "description": t.description, "parameters": t.schema}})
            })
            .collect())
    }

    pub async fn call_openai_tool(&self, names: &[String], fname: &str, args: Value) -> Result<String> {
        let tool = self
            .tools(names)
            .await?
            .into_iter()
            .find(|t| t.openai_name == fname)
            .ok_or_else(|| anyhow!("no tool named {fname}"))?;
        let c = self.client(&tool.server).await?;
        let r = c.request("tools/call", json!({"name": tool.name, "arguments": args}), CALL_TIMEOUT).await?;
        let text: Vec<String> = r["content"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or_default().to_string(),
                _ => b.to_string(),
            })
            .collect();
        let text = text.join("\n");
        if r["isError"].as_bool().unwrap_or(false) {
            bail!("{text}");
        }
        Ok(text)
    }

    /// Start a server and count its tools (for `senapati doctor`).
    pub async fn check(&self, name: &str) -> Result<usize> {
        Ok(self.client(name).await?.tools().await?.len())
    }
}

/// OpenAI function names: [a-zA-Z0-9_-], max 64 chars.
fn openai_name(server: &str, tool: &str) -> String {
    let s: String = format!("{server}__{tool}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    s.chars().take(64).collect()
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

struct McpClient {
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    tools: Mutex<Option<Vec<Value>>>,
}

impl McpClient {
    async fn start(cfg: &McpServerConfig) -> Result<Self> {
        let mut cmd = crate::proc::command(&cfg.command, &cfg.args)?;
        cmd.envs(&cfg.env).stderr(std::process::Stdio::null());
        let mut child = cmd.spawn()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped")));
        let stdout = child.stdout.take().expect("piped");
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let (p, w) = (pending.clone(), stdin.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if v.get("method").is_some() {
                    // Server-to-client request: answer ping, refuse the rest.
                    if let Some(id) = v.get("id") {
                        let reply = if v["method"] == "ping" {
                            json!({"jsonrpc": "2.0", "id": id, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported"}})
                        };
                        let _ = write_line(&w, &reply).await;
                    }
                    continue;
                }
                let Some(id) = v["id"].as_u64() else { continue };
                if let Some(tx) = p.lock().await.remove(&id) {
                    let r = match v.get("error") {
                        Some(e) => Err(anyhow!("mcp error: {e}")),
                        None => Ok(v["result"].clone()),
                    };
                    let _ = tx.send(r);
                }
            }
            p.lock().await.clear();
        });

        let c = Self {
            child: Mutex::new(child),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            tools: Mutex::new(None),
        };
        c.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "senapati", "version": env!("CARGO_PKG_VERSION")}}),
            START_TIMEOUT,
        )
        .await?;
        write_line(&c.stdin, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
        Ok(c)
    }

    async fn alive(&self) -> bool {
        matches!(self.child.lock().await.try_wait(), Ok(None))
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        write_line(&self.stdin, &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => bail!("mcp server closed"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("mcp {method} timed out")
            }
        }
    }

    async fn tools(&self) -> Result<Vec<Value>> {
        let mut cache = self.tools.lock().await;
        if let Some(t) = &*cache {
            return Ok(t.clone());
        }
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request("tools/list", params, START_TIMEOUT).await?;
            all.extend(r["tools"].as_array().cloned().unwrap_or_default());
            match r["nextCursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        *cache = Some(all.clone());
        Ok(all)
    }
}

async fn write_line(w: &Mutex<ChildStdin>, v: &Value) -> Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    let mut w = w.lock().await;
    w.write_all(s.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_names_are_sanitized() {
        assert_eq!(openai_name("rea", "open.binary"), "rea__open_binary");
        assert_eq!(openai_name("x", &"y".repeat(100)).len(), 64);
    }
}
