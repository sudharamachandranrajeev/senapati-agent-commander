use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const EXAMPLE_CONFIG: &str = include_str!("../config/senapati.example.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_agent")]
    pub default_agent: String,
    #[serde(default)]
    pub caveman: CavemanMode,
    #[serde(default)]
    pub skill_dirs: Vec<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub planner: PlannerConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
}

fn default_agent() -> String {
    "claude".into()
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CavemanMode {
    Off,
    Lite,
    Full,
    Ultra,
    #[default]
    Auto,
}

impl std::str::FromStr for CavemanMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "off" => Self::Off,
            "lite" => Self::Lite,
            "full" => Self::Full,
            "ultra" => Self::Ultra,
            "auto" => Self::Auto,
            _ => bail!("caveman mode must be off|lite|full|ultra|auto"),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "one")]
    pub max_concurrent: usize,
}

fn one() -> usize {
    1
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    ClaudeCli,
    CodexCli,
    OpenaiCompat,
    Cli,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub kind: AgentKind,
    pub provider: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_secret: Option<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
    #[serde(default)]
    pub fallback: Vec<String>,
    #[serde(default)]
    pub caveman: Option<CavemanMode>,
    /// Told to the planner, e.g. strengths or limits on this machine.
    #[serde(default)]
    pub notes: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Tools offered to local/OpenAI-compatible models (empty: all). Small models
    /// choke on hundreds of tool schemas. Claude and codex see every tool.
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PlannerConfig {
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NotifyConfig {
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub remote_control: bool,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub whatsapp: WhatsappConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub chat_ids: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DiscordConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WhatsappConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub sidecar_dir: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parse config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for a in &self.agents {
            if !seen.insert(a.name.as_str()) {
                bail!("duplicate agent name '{}'", a.name);
            }
            if a.kind == AgentKind::OpenaiCompat && (a.base_url.is_none() || a.model.is_none()) {
                bail!("agent '{}': openai-compat needs base_url and model", a.name);
            }
            if a.kind == AgentKind::Cli && a.command.is_none() {
                bail!("agent '{}': cli needs command", a.name);
            }
            for m in &a.mcp {
                if !self.mcp.contains_key(m) {
                    bail!("agent '{}': unknown mcp server '{m}'", a.name);
                }
            }
        }
        for a in &self.agents {
            for f in &a.fallback {
                if !self.agents.iter().any(|b| &b.name == f) {
                    bail!("agent '{}': unknown fallback '{f}'", a.name);
                }
            }
        }
        if !self.notify.whatsapp.to.iter().all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
            bail!("notify.whatsapp.to: numbers must be digits only, with country code");
        }
        Ok(())
    }

    pub fn agent(&self, name: &str) -> Option<&AgentConfig> {
        self.agents.iter().find(|a| a.name == name && a.enabled)
    }

    pub fn max_concurrent(&self, provider: &str) -> usize {
        self.providers.get(provider).map(|p| p.max_concurrent.max(1)).unwrap_or(1)
    }
}

/// Expand `~`, `{home}` and `{cwd}` in a configured path.
pub fn expand(path: &str, home: &Path, cwd: &Path) -> PathBuf {
    let s = path.replace("{home}", &home.to_string_lossy()).replace("{cwd}", &cwd.to_string_lossy());
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(s)
}

/// Always absolute: agents run in other folders and get paths under home.
pub fn resolve_home(flag: Option<PathBuf>) -> Result<PathBuf> {
    let home = match (flag, std::env::var("SENAPATI_HOME")) {
        (Some(h), _) => h,
        (None, Ok(h)) => PathBuf::from(h),
        (None, Err(_)) => dirs::data_local_dir().context("no LOCALAPPDATA folder")?.join("senapati"),
    };
    Ok(std::path::absolute(home)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let cfg = Config::parse(EXAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.default_agent, "claude");
        assert!(cfg.agent("claude").is_some());
        assert!(cfg.agent("gemini").is_none(), "disabled agents are hidden");
        assert_eq!(cfg.max_concurrent("anthropic"), 1);
        assert_eq!(cfg.max_concurrent("unknown"), 1);
    }

    #[test]
    fn rejects_unknown_fallback() {
        let bad = r#"
            [[agents]]
            name = "a"
            kind = "claude-cli"
            provider = "x"
            fallback = ["nope"]
        "#;
        assert!(Config::parse(bad).is_err());
    }

    #[test]
    fn rejects_bad_whatsapp_number() {
        let bad = "[notify.whatsapp]\nto = [\"+44 7700\"]\n";
        assert!(Config::parse(bad).is_err());
    }

    #[test]
    fn expands_tokens() {
        let p = expand("{home}/skills", Path::new("H"), Path::new("C"));
        assert_eq!(p, PathBuf::from("H/skills"));
        let p = expand("{cwd}/.claude/skills", Path::new("H"), Path::new("C"));
        assert_eq!(p, PathBuf::from("C/.claude/skills"));
    }
}
