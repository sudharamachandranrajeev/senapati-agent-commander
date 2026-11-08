//! The core: sessions, turns, provider concurrency, failover and alerts.
//! The TUI, `serve` and remote chat commands all drive this.

use crate::agents::{self, AgentEvent, LimitHit, RunCtx, TurnOutput, TurnRequest};
use crate::config::{expand, AgentKind, CavemanMode, Config};
use crate::layers::{self, Role};
use crate::mcp::McpHub;
use crate::notify::Notifier;
use crate::skills::{self, Skill};
use crate::store::Store;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio::sync::{broadcast, Semaphore};
use tokio::task::AbortHandle;

#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Sessions, messages or plans changed; views reload from the store.
    Changed,
    Notice(String),
}

pub type Sems = HashMap<String, Arc<Semaphore>>;

pub struct TurnInput {
    pub prompt: String,
    pub role: Role,
    pub skill: Option<String>,
}

pub struct Engine {
    pub cfg: Config,
    pub home: PathBuf,
    pub store: Store,
    pub events: broadcast::Sender<AppEvent>,
    sems: Sems,
    ctx: RunCtx,
    caveman: RwLock<CavemanMode>,
    pub default_cwd: RwLock<PathBuf>,
    turns: Mutex<HashMap<String, AbortHandle>>,
    pub(crate) plan_tasks: Mutex<HashMap<String, AbortHandle>>,
    pub notifier: OnceLock<Arc<Notifier>>,
}

impl Engine {
    pub fn new(cfg: Config, home: PathBuf, store: Store) -> Arc<Self> {
        let sems = provider_sems(&cfg, None);
        let (events, _) = broadcast::channel(512);
        let mcp = Arc::new(McpHub::new(cfg.mcp.clone()));
        Arc::new(Self {
            caveman: RwLock::new(cfg.caveman),
            ctx: RunCtx { home: home.clone(), mcp },
            sems,
            events,
            store,
            home,
            cfg,
            default_cwd: RwLock::new(std::env::current_dir().unwrap_or_else(|_| ".".into())),
            turns: Mutex::new(HashMap::new()),
            plan_tasks: Mutex::new(HashMap::new()),
            notifier: OnceLock::new(),
        })
    }

    pub fn mcp(&self) -> &McpHub {
        &self.ctx.mcp
    }

    pub fn emit(&self, e: AppEvent) {
        let _ = self.events.send(e);
    }

    pub fn notice(&self, s: impl Into<String>) {
        self.emit(AppEvent::Notice(s.into()));
    }

    pub fn caveman(&self) -> CavemanMode {
        *self.caveman.read().unwrap()
    }

    pub fn set_caveman(&self, m: CavemanMode) {
        *self.caveman.write().unwrap() = m;
    }

    pub fn cwd(&self) -> PathBuf {
        self.default_cwd.read().unwrap().clone()
    }

    /// Send an alert to the chat channels if the event kind is enabled.
    pub fn alert(&self, kind: &str, text: String) {
        if !self.cfg.notify.events.iter().any(|e| e == kind) {
            return;
        }
        if let Some(n) = self.notifier.get() {
            let n = n.clone();
            tokio::spawn(async move { n.send(&text).await });
        }
    }

    pub fn skills(&self, cwd: &Path) -> Vec<Skill> {
        let dirs: Vec<PathBuf> = self.cfg.skill_dirs.iter().map(|d| expand(d, &self.home, cwd)).collect();
        skills::discover(&dirs)
    }

    /// Running/limit per provider, for the agents view.
    pub fn provider_load(&self) -> Vec<(String, usize, usize)> {
        let mut v: Vec<_> = self
            .sems
            .iter()
            .map(|(p, s)| {
                let max = self.cfg.max_concurrent(p);
                (p.clone(), max - s.available_permits().min(max), max)
            })
            .collect();
        v.sort();
        v
    }

    pub fn new_session(&self, agent: Option<&str>, title: &str, cwd: Option<PathBuf>) -> Result<String> {
        let agent = agent.unwrap_or(&self.cfg.default_agent);
        self.cfg.agent(agent).ok_or_else(|| anyhow!("no enabled agent '{agent}'"))?;
        let cwd = cwd.unwrap_or_else(|| self.cwd());
        let s = self.store.create_session(title, agent, &cwd.to_string_lossy())?;
        self.emit(AppEvent::Changed);
        Ok(s.id)
    }

    /// Start a chat turn in the background. A second message while one runs is refused.
    pub fn send(self: &Arc<Self>, session_id: &str, input: TurnInput) -> Result<()> {
        let mut turns = self.turns.lock().unwrap();
        if turns.get(session_id).is_some_and(|h| !h.is_finished()) {
            return Err(anyhow!("session {session_id} is busy; /stop it first"));
        }
        let (me, id) = (self.clone(), session_id.to_string());
        let task = tokio::spawn(async move {
            let title = me.store.session(&id).ok().flatten().map(|s| s.title).unwrap_or_default();
            match me.turn(&id, input, None).await {
                Ok(out) => me.alert("turn_done", format!("✅ {title} [{id}]\n{}", agents::tail(&out.text, 600))),
                Err(e) => me.notice(format!("{id}: {e:#}")),
            }
        });
        turns.insert(session_id.to_string(), task.abort_handle());
        Ok(())
    }

    pub fn stop(&self, session_id: &str) -> bool {
        let h = self.turns.lock().unwrap().remove(session_id);
        match h {
            Some(h) if !h.is_finished() => {
                h.abort();
                let _ = self.store.add_message(session_id, "system", "stopped by user");
                let _ = self.store.set_session_status(session_id, "idle");
                self.emit(AppEvent::Changed);
                true
            }
            _ => false,
        }
    }

    /// Run one turn to completion, waiting for a provider slot and failing over on limits.
    pub async fn turn(&self, session_id: &str, input: TurnInput, sems: Option<&Sems>) -> Result<TurnOutput> {
        self.store.add_message(session_id, "user", &input.prompt)?;
        self.emit(AppEvent::Changed);
        let mut tried: Vec<String> = Vec::new();
        let mut prompt = input.prompt.clone();
        loop {
            let session = self.store.session(session_id)?.context("session gone")?;
            let agent = self.cfg.agent(&session.agent).ok_or_else(|| anyhow!("agent '{}' not enabled", session.agent))?.clone();
            tried.push(agent.name.clone());
            let cwd = PathBuf::from(&session.cwd);

            let sem = sems.unwrap_or(&self.sems).get(&agent.provider).cloned().unwrap_or_else(|| Arc::new(Semaphore::new(1)));
            self.set_status(session_id, "queued");
            let permit = sem.acquire_owned().await?;
            self.set_status(session_id, "running");

            let skill = match &input.skill {
                Some(name) => Some(self.skills(&cwd).into_iter().find(|s| &s.name == name).ok_or_else(|| anyhow!("unknown skill '{name}'"))?),
                None => None,
            };
            let system = layers::build(self.caveman(), input.role, skill.as_ref(), &agent, &cwd);
            let mut history = self.store.messages(session_id)?;
            history.pop(); // the prompt itself
            let req = TurnRequest { prompt: prompt.clone(), system, cwd, resume: session.vendor_id.clone(), history };

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let sink = {
                let (store, events, id) = (&self.store, &self.events, session_id.to_string());
                async move {
                    while let Some(ev) = rx.recv().await {
                        let _ = match ev {
                            AgentEvent::VendorId(v) => store.set_vendor_id(&id, &v),
                            AgentEvent::Text(t) => store.add_message(&id, "assistant", &t),
                            AgentEvent::Tool(t) => store.add_message(&id, "tool", &t),
                        };
                        let _ = events.send(AppEvent::Changed);
                    }
                }
            };
            let (res, _) = tokio::join!(agents::run(&agent, &self.ctx, req, tx), sink);
            drop(permit);

            match res {
                Ok(out) => {
                    if let Some(v) = &out.vendor_id {
                        self.store.set_vendor_id(session_id, v)?;
                    }
                    self.set_status(session_id, "idle");
                    return Ok(out);
                }
                Err(e) if e.downcast_ref::<LimitHit>().is_some() => {
                    self.alert("limit_hit", format!("⛔ {} hit a limit in session {session_id}: {}", agent.name, agents::one_line(&e.to_string(), 200)));
                    let next = agent.fallback.iter().find(|f| !tried.contains(f) && self.cfg.agent(f).is_some()).cloned();
                    let Some(next) = next else {
                        self.fail_turn(session_id, &e);
                        return Err(e);
                    };
                    self.store.add_message(session_id, "system", &format!("{} hit a limit; failing over to {next}", agent.name))?;
                    self.store.set_session_agent(session_id, &next)?;
                    let next_kind = self.cfg.agent(&next).map(|a| a.kind);
                    if matches!(next_kind, Some(AgentKind::ClaudeCli | AgentKind::CodexCli)) {
                        prompt = with_earlier_context(&self.store.messages(session_id)?, &input.prompt);
                    }
                }
                Err(e) => {
                    self.fail_turn(session_id, &e);
                    return Err(e);
                }
            }
        }
    }

    fn fail_turn(&self, session_id: &str, e: &anyhow::Error) {
        let _ = self.store.add_message(session_id, "error", &format!("{e:#}"));
        self.set_status(session_id, "error");
    }

    fn set_status(&self, id: &str, s: &str) {
        let _ = self.store.set_session_status(id, s);
        self.emit(AppEvent::Changed);
    }
}

pub fn provider_sems(cfg: &Config, per_provider: Option<usize>) -> Sems {
    cfg.agents
        .iter()
        .map(|a| {
            let n = per_provider.unwrap_or_else(|| cfg.max_concurrent(&a.provider)).max(1);
            (a.provider.clone(), Arc::new(Semaphore::new(n)))
        })
        .collect()
}

/// A failover agent starts with no memory of the session: replay recent turns.
fn with_earlier_context(msgs: &[crate::store::Message], prompt: &str) -> String {
    let turns: Vec<_> = msgs.iter().filter(|m| m.role == "user" || m.role == "assistant").collect();
    let recent = &turns[turns.len().saturating_sub(11)..turns.len().saturating_sub(1)];
    if recent.is_empty() {
        return prompt.to_string();
    }
    let mut s = String::from("<earlier-conversation note=\"another agent handled these turns\">\n");
    for m in recent {
        s.push_str(&format!("[{}] {}\n", m.role, agents::tail(&m.content, 2000)));
    }
    s.push_str("</earlier-conversation>\n\n");
    s.push_str(prompt);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn limit_hit_fails_over_to_next_agent() {
        let cfg = Config::parse(
            r#"
            default_agent = "a"
            caveman = "off"
            [[agents]]
            name = "a"
            kind = "cli"
            provider = "p1"
            command = "powershell"
            args = ["-NoProfile", "-Command", "[Console]::Error.WriteLine('Claude usage limit reached'); exit 1"]
            fallback = ["b"]
            [[agents]]
            name = "b"
            kind = "cli"
            provider = "p2"
            command = "powershell"
            args = ["-NoProfile", "-Command", "'fallback answered'"]
            "#,
        )
        .unwrap();
        let e = Engine::new(cfg, std::env::temp_dir(), Store::memory().unwrap());
        let sid = e.new_session(None, "t", None).unwrap();
        let out = e.turn(&sid, TurnInput { prompt: "hi".into(), role: Role::Chat, skill: None }, None).await.unwrap();
        assert_eq!(out.text, "fallback answered");
        let s = e.store.session(&sid).unwrap().unwrap();
        assert_eq!(s.agent, "b");
        assert_eq!(s.status, "idle");
        let msgs = e.store.messages(&sid).unwrap();
        assert!(msgs.iter().any(|m| m.role == "system" && m.content.contains("failing over to b")));
    }

    #[tokio::test]
    async fn plain_errors_do_not_fail_over() {
        let cfg = Config::parse(
            r#"
            default_agent = "a"
            [[agents]]
            name = "a"
            kind = "cli"
            provider = "p1"
            command = "powershell"
            args = ["-NoProfile", "-Command", "[Console]::Error.WriteLine('file not found'); exit 1"]
            fallback = ["b"]
            [[agents]]
            name = "b"
            kind = "cli"
            provider = "p2"
            command = "powershell"
            args = ["-NoProfile", "-Command", "'should not run'"]
            "#,
        )
        .unwrap();
        let e = Engine::new(cfg, std::env::temp_dir(), Store::memory().unwrap());
        let sid = e.new_session(None, "t", None).unwrap();
        assert!(e.turn(&sid, TurnInput { prompt: "hi".into(), role: Role::Chat, skill: None }, None).await.is_err());
        let s = e.store.session(&sid).unwrap().unwrap();
        assert_eq!((s.agent.as_str(), s.status.as_str()), ("a", "error"));
    }
}
