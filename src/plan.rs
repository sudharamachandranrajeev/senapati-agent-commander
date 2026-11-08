//! Delegation plans: a planner agent splits a goal into steps, the user approves,
//! the executor runs steps as their dependencies finish. Steps on different
//! providers run in parallel; each provider runs one at a time unless raised.

use crate::agents;
use crate::config::AgentKind;
use crate::engine::{provider_sems, AppEvent, Engine, TurnInput};
use crate::layers::{extract_handoff, step_failure, Role};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Drafting,
    AwaitingApproval,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub title: String,
    pub agent: String,
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub prompt: String,
    #[serde(default)]
    pub status: StepStatus,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub handoff: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub goal: String,
    pub cwd: String,
    pub status: PlanStatus,
    #[serde(default)]
    pub per_provider: Option<usize>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Plan {
    pub fn finished(&self) -> bool {
        matches!(self.status, PlanStatus::Done | PlanStatus::Failed | PlanStatus::Cancelled)
    }
}

pub fn load(engine: &Engine, id: &str) -> Result<Plan> {
    let json = engine.store.plan(id)?.ok_or_else(|| anyhow!("no plan '{id}'"))?;
    Ok(serde_json::from_str(&json)?)
}

pub fn all(engine: &Engine) -> Vec<Plan> {
    engine
        .store
        .plans()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|json| serde_json::from_str(&json).ok())
        .collect()
}

fn save(engine: &Engine, p: &Plan) -> Result<()> {
    engine.store.save_plan(&p.id, &serde_json::to_string_pretty(p)?)?;
    engine.emit(AppEvent::Changed);
    Ok(())
}

/// Create a plan and start drafting it in the background. Returns its id.
pub fn create(engine: &Arc<Engine>, goal: &str, cwd: Option<PathBuf>, per_provider: Option<usize>) -> Result<String> {
    let id = format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..5]);
    let plan = Plan {
        id: id.clone(),
        goal: goal.to_string(),
        cwd: cwd.unwrap_or_else(|| engine.cwd()).to_string_lossy().to_string(),
        status: PlanStatus::Drafting,
        per_provider,
        notes: vec![],
        steps: vec![],
    };
    save(engine, &plan)?;
    let (e, pid) = (engine.clone(), id.clone());
    tokio::spawn(async move {
        if let Err(err) = draft(&e, &pid).await {
            if let Ok(mut p) = load(&e, &pid) {
                p.status = PlanStatus::Failed;
                p.notes.push(format!("planning failed: {err:#}"));
                let _ = save(&e, &p);
            }
            e.alert("step_failed", format!("❌ planning {pid} failed: {}", agents::one_line(&format!("{err:#}"), 300)));
        }
    });
    Ok(id)
}

async fn draft(engine: &Arc<Engine>, id: &str) -> Result<()> {
    let mut plan = load(engine, id)?;
    let cwd = PathBuf::from(&plan.cwd);
    let planner = engine.cfg.planner.agent.clone().unwrap_or_else(|| engine.cfg.default_agent.clone());
    let sid = engine.new_session(Some(&planner), &format!("{id} · planner"), Some(cwd.clone()))?;
    let prompt = planner_prompt(engine, &plan.goal, &cwd);
    let out = engine.turn(&sid, TurnInput { prompt, role: Role::Planner, skill: None }, None).await?;
    let (steps, notes) = normalize(engine, parse_steps(&out.text)?, &cwd)?;
    plan = load(engine, id)?;
    plan.steps = steps;
    plan.notes.extend(notes);
    plan.status = PlanStatus::AwaitingApproval;
    save(engine, &plan)?;
    engine.alert(
        "plan_ready",
        format!("📋 plan {id} ready ({} steps): {}\n{}\nApprove: /approve {id}", plan.steps.len(), plan.goal, summary(&plan)),
    );
    Ok(())
}

fn planner_prompt(engine: &Engine, goal: &str, cwd: &std::path::Path) -> String {
    let mut s = format!(
        "Plan only. Do not do the work and do not use tools.\n\
         Break this goal into a delegation plan for the agents below.\n\nGoal: {goal}\nWorking folder: {}\n\nAgents (name · kind · provider · abilities):\n",
        cwd.display()
    );
    for a in engine.cfg.agents.iter().filter(|a| a.enabled) {
        let ability = match a.kind {
            AgentKind::ClaudeCli | AgentKind::CodexCli => "coding agent: reads/edits files, runs shell",
            AgentKind::OpenaiCompat => "local/remote model: text only, no file or shell access",
            AgentKind::Cli => "CLI agent",
        };
        let tools = if a.mcp.is_empty() { String::new() } else { format!(" · MCP: {}", a.mcp.join(", ")) };
        let notes = a.notes.as_ref().map(|n| format!(" · NOTE: {n}")).unwrap_or_default();
        s.push_str(&format!("- {} · {:?} · {} · {ability}{tools}{notes}\n", a.name, a.kind, a.provider));
    }
    let skills = engine.skills(cwd);
    if !skills.is_empty() {
        s.push_str("\nSkills (name: description):\n");
        for k in skills {
            s.push_str(&format!("- {}: {}\n", k.name, agents::one_line(&k.description, 200)));
        }
    }
    s.push_str(
        "\nRules:\n\
         - An agent's NOTE is a hard limit: never give an agent work its NOTE rules out, even if that means less parallelism.\n\
         - Each provider runs one agent at a time. Steps on different providers run in parallel, so spread independent work across providers when the agents can do it.\n\
         - Text-only models get self-contained text tasks (drafting, summarising, reviewing text given in the prompt).\n\
         - 1 to 8 steps. Each prompt must be self-contained: an agent sees only its prompt plus the handoffs of the steps in its depends_on.\n\
         - Set skill only when one clearly fits; otherwise null.\n\
         Reply with only a JSON object in a ```json fence:\n\
         {\"steps\":[{\"id\":\"s1\",\"title\":\"...\",\"agent\":\"<agent name>\",\"skill\":null,\"depends_on\":[],\"prompt\":\"...\"}]}\n",
    );
    s
}

pub fn parse_steps(text: &str) -> Result<Vec<Step>> {
    let body = match text.find("```json") {
        Some(i) => {
            let rest = &text[i + 7..];
            &rest[..rest.find("```").unwrap_or(rest.len())]
        }
        None => {
            let (a, b) = (text.find('{'), text.rfind('}'));
            match (a, b) {
                (Some(a), Some(b)) if b > a => &text[a..=b],
                _ => bail!("planner reply has no JSON"),
            }
        }
    };
    #[derive(Deserialize)]
    struct Wrap {
        steps: Vec<Step>,
    }
    let w: Wrap = serde_json::from_str(body.trim()).context("planner JSON does not match the plan schema")?;
    Ok(w.steps)
}

/// Fix what can be fixed (unknown agent or skill), reject what cannot (bad deps, cycles).
fn normalize(engine: &Engine, mut steps: Vec<Step>, cwd: &std::path::Path) -> Result<(Vec<Step>, Vec<String>)> {
    let mut notes = Vec::new();
    if steps.is_empty() || steps.len() > 20 {
        bail!("plan needs 1-20 steps, got {}", steps.len());
    }
    let skills: HashSet<String> = engine.skills(cwd).into_iter().map(|s| s.name).collect();
    let mut ids = HashSet::new();
    for s in steps.iter_mut() {
        if s.id.trim().is_empty() || !ids.insert(s.id.clone()) {
            bail!("step ids must be unique and non-empty ('{}')", s.id);
        }
        if engine.cfg.agent(&s.agent).is_none() {
            notes.push(format!("{}: unknown agent '{}', using {}", s.id, s.agent, engine.cfg.default_agent));
            s.agent = engine.cfg.default_agent.clone();
        }
        if let Some(k) = &s.skill {
            if !skills.contains(k) {
                notes.push(format!("{}: unknown skill '{k}' dropped", s.id));
                s.skill = None;
            }
        }
        s.status = StepStatus::Pending;
        s.session_id = None;
        s.handoff = None;
        s.error = None;
    }
    check_graph(&steps)?;
    Ok((steps, notes))
}

pub fn check_graph(steps: &[Step]) -> Result<()> {
    let ids: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    for s in steps {
        for d in &s.depends_on {
            if d == &s.id || !ids.contains(d.as_str()) {
                bail!("step {} has a bad dependency '{d}'", s.id);
            }
        }
    }
    // Kahn: every step must become ready eventually.
    let mut done: HashSet<&str> = HashSet::new();
    loop {
        let before = done.len();
        for s in steps {
            if !done.contains(s.id.as_str()) && s.depends_on.iter().all(|d| done.contains(d.as_str())) {
                done.insert(&s.id);
            }
        }
        if done.len() == steps.len() {
            return Ok(());
        }
        if done.len() == before {
            bail!("plan has a dependency cycle");
        }
    }
}

pub fn summary(p: &Plan) -> String {
    p.steps
        .iter()
        .map(|s| {
            let deps = if s.depends_on.is_empty() { String::new() } else { format!(" ← {}", s.depends_on.join(",")) };
            format!("{} [{}] {}{deps}", s.id, s.agent, s.title)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn approve(engine: &Arc<Engine>, id: &str) -> Result<()> {
    let mut plan = load(engine, id)?;
    if plan.status != PlanStatus::AwaitingApproval {
        bail!("plan {id} is {:?}, not awaiting approval", plan.status);
    }
    plan.status = PlanStatus::Running;
    save(engine, &plan)?;
    let (e, pid) = (engine.clone(), id.to_string());
    let task = tokio::spawn(async move {
        if let Err(err) = execute(&e, &pid).await {
            e.notice(format!("plan {pid}: {err:#}"));
        }
    });
    engine.plan_tasks.lock().unwrap().insert(id.to_string(), task.abort_handle());
    Ok(())
}

pub fn cancel(engine: &Engine, id: &str) -> Result<()> {
    if let Some(h) = engine.plan_tasks.lock().unwrap().remove(id) {
        h.abort();
    }
    let mut plan = load(engine, id)?;
    if plan.finished() {
        bail!("plan {id} already finished");
    }
    for s in plan.steps.iter_mut().filter(|s| s.status == StepStatus::Running) {
        s.status = StepStatus::Failed;
        s.error = Some("cancelled".into());
        if let Some(sid) = &s.session_id {
            let _ = engine.store.set_session_status(sid, "idle");
        }
    }
    plan.status = PlanStatus::Cancelled;
    save(engine, &plan)
}

/// Replace a waiting plan's steps from an edited JSON file.
pub fn import_edit(engine: &Engine, id: &str, json: &str) -> Result<()> {
    let mut plan = load(engine, id)?;
    if plan.status != PlanStatus::AwaitingApproval {
        bail!("only a plan awaiting approval can be edited");
    }
    let edited: Plan = serde_json::from_str(json).context("edited plan is not valid JSON for a plan")?;
    let (steps, notes) = normalize(engine, edited.steps, std::path::Path::new(&plan.cwd))?;
    plan.steps = steps;
    plan.notes.extend(notes);
    plan.per_provider = edited.per_provider;
    save(engine, &plan)
}

fn step_prompt(plan: &Plan, step: &Step) -> String {
    let mut s = format!("{}\n\nOverall goal (context only): {}", step.prompt, plan.goal);
    let deps: Vec<&Step> = plan.steps.iter().filter(|d| step.depends_on.contains(&d.id)).collect();
    if !deps.is_empty() {
        s.push_str("\n\nHandoffs from earlier steps:");
        for d in deps {
            s.push_str(&format!("\n[{} · {}] {}", d.id, d.title, d.handoff.as_deref().unwrap_or("(none)")));
        }
    }
    s.push_str("\n\nFinish with `HANDOFF:` if you did the step, or `STEP FAILED:` and the reason if you could not.");
    s
}

async fn execute(engine: &Arc<Engine>, id: &str) -> Result<()> {
    let first = load(engine, id)?;
    let sems = first.per_provider.map(|n| provider_sems(&engine.cfg, Some(n)));
    let mut set: tokio::task::JoinSet<(String, Result<String>)> = tokio::task::JoinSet::new();
    loop {
        let mut plan = load(engine, id)?;
        let state: HashMap<String, StepStatus> = plan.steps.iter().map(|s| (s.id.clone(), s.status)).collect();
        let mut changed = false;
        for s in plan.steps.iter_mut().filter(|s| s.status == StepStatus::Pending) {
            if s.depends_on.iter().any(|d| matches!(state.get(d), Some(StepStatus::Failed | StepStatus::Skipped))) {
                s.status = StepStatus::Skipped;
                changed = true;
            }
        }
        let snapshot = plan.clone();
        for s in plan.steps.iter_mut().filter(|s| s.status == StepStatus::Pending) {
            if !s.depends_on.iter().all(|d| state.get(d) == Some(&StepStatus::Done)) {
                continue;
            }
            let title = format!("{id}·{} {}", s.id, s.title);
            let sid = engine.new_session(Some(&s.agent), &title, Some(PathBuf::from(&plan.cwd)))?;
            s.session_id = Some(sid.clone());
            s.status = StepStatus::Running;
            changed = true;
            let input = TurnInput { prompt: step_prompt(&snapshot, s), role: Role::Step, skill: s.skill.clone() };
            let (e, sems, step_id) = (engine.clone(), sems.clone(), s.id.clone());
            set.spawn(async move {
                let r = e.turn(&sid, input, sems.as_ref()).await.and_then(|o| match step_failure(&o.text) {
                    Some(reason) => Err(anyhow!("agent reported: {reason}")),
                    None => Ok(o.text),
                });
                (step_id, r)
            });
        }
        if changed {
            save(engine, &plan)?;
        }
        let Some(joined) = set.join_next().await else { break };
        let (step_id, result) = joined?;
        let mut plan = load(engine, id)?;
        let step = plan.steps.iter_mut().find(|s| s.id == step_id).context("step vanished")?;
        match result {
            Ok(text) => {
                step.status = StepStatus::Done;
                step.handoff = Some(extract_handoff(&text));
                engine.alert("step_done", format!("✔ {id}·{} {} done", step.id, step.title));
            }
            Err(err) => {
                step.status = StepStatus::Failed;
                step.error = Some(format!("{err:#}"));
                engine.alert("step_failed", format!("❌ {id}·{} {} failed: {}", step.id, step.title, agents::one_line(&format!("{err:#}"), 300)));
            }
        }
        save(engine, &plan)?;
    }
    let mut plan = load(engine, id)?;
    let ok = plan.steps.iter().all(|s| s.status == StepStatus::Done);
    plan.status = if ok { PlanStatus::Done } else { PlanStatus::Failed };
    save(engine, &plan)?;
    engine.plan_tasks.lock().unwrap().remove(id);
    let counts = |st: StepStatus| plan.steps.iter().filter(|s| s.status == st).count();
    engine.alert(
        "plan_done",
        format!(
            "{} plan {id} {}: {} done, {} failed, {} skipped\n{}",
            if ok { "🏁" } else { "⚠" },
            if ok { "finished" } else { "ended with failures" },
            counts(StepStatus::Done),
            counts(StepStatus::Failed),
            counts(StepStatus::Skipped),
            plan.goal
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, deps: &[&str]) -> Step {
        Step {
            id: id.into(),
            title: id.into(),
            agent: "claude".into(),
            skill: None,
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            prompt: "p".into(),
            status: StepStatus::Pending,
            session_id: None,
            handoff: None,
            error: None,
        }
    }

    #[test]
    fn parses_fenced_json() {
        let t = "Here:\n```json\n{\"steps\":[{\"id\":\"s1\",\"title\":\"t\",\"agent\":\"codex\",\"prompt\":\"do\"}]}\n```";
        let s = parse_steps(t).unwrap();
        assert_eq!(s[0].agent, "codex");
        assert!(s[0].depends_on.is_empty());
    }

    #[test]
    fn parses_bare_json() {
        let s = parse_steps("{\"steps\":[{\"id\":\"a\",\"title\":\"t\",\"agent\":\"x\",\"prompt\":\"p\",\"skill\":null}]}").unwrap();
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn graph_checks() {
        assert!(check_graph(&[step("a", &[]), step("b", &["a"])]).is_ok());
        assert!(check_graph(&[step("a", &["b"]), step("b", &["a"])]).is_err());
        assert!(check_graph(&[step("a", &["zzz"])]).is_err());
        assert!(check_graph(&[step("a", &["a"])]).is_err());
    }
}
