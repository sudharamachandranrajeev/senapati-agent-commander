//! System-prompt layers added to every provider: caveman output style,
//! the active skill, and the plan-step handoff contract.

use crate::config::{AgentConfig, AgentKind, CavemanMode};
use crate::skills::Skill;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Chat,
    Planner,
    Step,
}

/// Resolve `auto`: people read chat and step output; the planner only feeds a machine.
pub fn level(mode: CavemanMode, role: Role) -> CavemanMode {
    match (mode, role) {
        (CavemanMode::Auto, Role::Planner) => CavemanMode::Ultra,
        (CavemanMode::Auto, _) => CavemanMode::Full,
        (m, _) => m,
    }
}

const LITE: &str = "Output style: concise. No greeting, filler, hedging, recap or closing offer. \
Full sentences are fine. Keep every technical detail exact.";

const FULL: &str = "Output style (caveman): answer first, then reason, then next step. \
No greeting, filler, hedging, recap or closing offer. Drop articles and filler words when the \
sentence still reads in one pass; fragments are fine. Prefer short common words. Keep every \
technical fact. Code, commands, paths, numbers and error text stay exact. Never drop \
not/no/never/only. Use normal full prose for security warnings, irreversible actions, ordered \
steps a fragment could scramble, and anything written to files or sent to people \
(code comments, docs, commits, messages).";

const ULTRA_EXTRA: &str = " Ultra: telegraphic. Lists and key: value lines, no full sentences, \
standard abbreviations only (DB, API, HTTP).";

pub fn caveman_text(level: CavemanMode) -> String {
    match level {
        CavemanMode::Off | CavemanMode::Auto => String::new(),
        CavemanMode::Lite => LITE.into(),
        CavemanMode::Full => FULL.into(),
        CavemanMode::Ultra => format!("{FULL}{ULTRA_EXTRA}"),
    }
}

pub const HANDOFF_MARK: &str = "HANDOFF:";

pub const FAILED_MARK: &str = "STEP FAILED:";

const STEP: &str = "You are running one step of a larger delegated plan. Do only this step. \
When finished, end your reply with a line `HANDOFF:` followed by at most 120 words, telegraphic: \
what you did, files changed, facts and open issues the next steps need. Later steps see only \
that handoff, not the rest of your reply. If you could not complete the step (blocked, no \
permission, missing input), end instead with a line `STEP FAILED:` and the reason. Never \
invent results you did not produce.";

/// Claude Code loads skills itself from these folders; others need the body inline.
fn claude_native(skill: &Skill, cwd: &Path) -> bool {
    let native = [Some(cwd.join(".claude").join("skills")), dirs::home_dir().map(|h| h.join(".claude").join("skills"))];
    native.iter().flatten().any(|d| skill.dir.starts_with(d))
}

pub fn skill_text(skill: &Skill, agent: &AgentConfig, cwd: &Path) -> String {
    if agent.kind == AgentKind::ClaudeCli && claude_native(skill, cwd) {
        return format!("Use the `{}` skill for this task.", skill.name);
    }
    format!(
        "Skill `{}` is active. Follow its instructions. Its folder, with any scripts and data \
         it mentions, is: {}\n\n<skill name=\"{}\">\n{}\n</skill>",
        skill.name,
        skill.dir.display(),
        skill.name,
        skill.body
    )
}

pub fn build(mode: CavemanMode, role: Role, skill: Option<&Skill>, agent: &AgentConfig, cwd: &Path) -> String {
    let mode = agent.caveman.unwrap_or(mode);
    let mut parts = Vec::new();
    let style = caveman_text(level(mode, role));
    if !style.is_empty() {
        parts.push(style);
    }
    if role == Role::Step {
        parts.push(STEP.into());
    }
    if let Some(s) = skill {
        parts.push(skill_text(s, agent, cwd));
    }
    parts.join("\n\n")
}

/// The agent's own failure report, when it ended with `STEP FAILED:` rather than a handoff.
pub fn step_failure(text: &str) -> Option<String> {
    let f = text.rfind(FAILED_MARK)?;
    if text.rfind(HANDOFF_MARK).is_some_and(|h| h > f) {
        return None;
    }
    Some(text[f + FAILED_MARK.len()..].trim().to_string())
}

/// The handoff block of a step reply, or its tail when the agent skipped the contract.
pub fn extract_handoff(text: &str) -> String {
    match text.rfind(HANDOFF_MARK) {
        Some(i) => text[i + HANDOFF_MARK.len()..].trim().to_string(),
        None => crate::agents::tail(text, 1200),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(kind: AgentKind) -> AgentConfig {
        let mut cfg = crate::config::Config::parse(crate::config::EXAMPLE_CONFIG).unwrap();
        let mut a = cfg.agents.remove(0);
        a.kind = kind;
        a
    }

    #[test]
    fn auto_levels() {
        assert_eq!(level(CavemanMode::Auto, Role::Chat), CavemanMode::Full);
        assert_eq!(level(CavemanMode::Auto, Role::Planner), CavemanMode::Ultra);
        assert_eq!(level(CavemanMode::Off, Role::Planner), CavemanMode::Off);
    }

    #[test]
    fn step_layer_has_handoff_contract() {
        let s = build(CavemanMode::Off, Role::Step, None, &agent(AgentKind::CodexCli), Path::new("."));
        assert!(s.contains("HANDOFF:"));
        assert!(!s.contains("caveman"));
    }

    #[test]
    fn skill_inline_for_non_claude() {
        let sk = Skill { name: "x".into(), description: "".into(), dir: "C:/s/x".into(), body: "BODY".into() };
        assert!(skill_text(&sk, &agent(AgentKind::OpenaiCompat), Path::new(".")).contains("BODY"));
        let native = Skill { dir: Path::new("proj").join(".claude/skills/x"), ..sk };
        let t = skill_text(&native, &agent(AgentKind::ClaudeCli), Path::new("proj"));
        assert!(!t.contains("BODY"));
    }

    #[test]
    fn step_failure_detection() {
        assert_eq!(step_failure("tried\nSTEP FAILED: read-only sandbox").as_deref(), Some("read-only sandbox"));
        assert!(step_failure("STEP FAILED: x then fixed\nHANDOFF: done").is_none());
        assert!(step_failure("HANDOFF: ok").is_none());
    }

    #[test]
    fn handoff_extraction() {
        assert_eq!(extract_handoff("work...\nHANDOFF: did x; file a.rs"), "did x; file a.rs");
        assert_eq!(extract_handoff("no mark"), "no mark");
    }
}
