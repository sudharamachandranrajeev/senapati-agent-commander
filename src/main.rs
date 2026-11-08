//! Senapati (Agent-Commander): drive coding agents and local models from one Windows terminal.

mod agents;
mod config;
mod doctor;
mod engine;
mod layers;
mod mcp;
mod notify;
mod plan;
mod proc;
mod remote;
mod secrets;
mod skills;
mod store;
mod tui;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use engine::{Engine, TurnInput};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "senapati", version, about = "Senapati (Agent-Commander): agents, local models, delegation plans, chat alerts")]
struct Cli {
    /// Data folder (config, database, WhatsApp link). Default: %SENAPATI_HOME% or %LOCALAPPDATA%\senapati
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the terminal UI (default)
    Tui,
    /// Create the home folder and a starter config
    Init {
        /// Skills folder to copy into <home>\skills (default: .\skills if present)
        #[arg(long)]
        skills: Option<PathBuf>,
    },
    /// Check config, agents, local servers, MCP servers, skills and alert channels
    Doctor,
    /// One turn with an agent, printed to stdout
    Ask {
        agent: String,
        #[arg(long)]
        skill: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Continue an existing session (resumes the vendor session)
    Reply {
        session: String,
        #[arg(required = true, trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Draft a delegation plan; with --yes, run it too
    Plan {
        #[arg(long)]
        yes: bool,
        /// Agents allowed at once per provider for this plan (default: config, normally 1)
        #[arg(long)]
        per_provider: Option<usize>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true)]
        goal: Vec<String>,
    },
    /// Run a drafted plan and wait for it
    Approve { id: String },
    /// Headless: alerts plus Telegram/WhatsApp remote commands, no UI
    Serve,
    /// Store secrets in Windows Credential Manager
    Secret {
        #[command(subcommand)]
        op: SecretOp,
    },
    /// Link the WhatsApp bot account (QR code)
    Whatsapp {
        #[command(subcommand)]
        op: WhatsappOp,
    },
    /// Send a test alert to every enabled channel
    NotifyTest,
    /// List discovered skills
    Skills,
}

#[derive(Subcommand)]
enum SecretOp {
    /// Prompt for a value without echo and store it
    Set { name: String },
    Delete { name: String },
    /// Show which known secrets are set
    List,
}

#[derive(Subcommand)]
enum WhatsappOp {
    Link,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let home = config::resolve_home(cli.home)?;
    let cmd = cli.cmd.unwrap_or(Cmd::Tui);

    match &cmd {
        Cmd::Init { skills } => return init(&home, skills.clone()),
        Cmd::Secret { op } => return secret(op),
        _ => {}
    }

    let cfg_path = home.join("senapati.toml");
    if !cfg_path.exists() {
        bail!("no config at {}; run `senapati init` first", cfg_path.display());
    }
    let cfg = config::Config::load(&cfg_path)?;
    std::fs::create_dir_all(&home)?;
    let store = store::Store::open(&home.join("senapati.db"))?;
    let engine = Engine::new(cfg, home, store);

    match cmd {
        Cmd::Tui => {
            start_notifier(&engine, true).await;
            tui::run(engine).await
        }
        Cmd::Doctor => {
            let fails = doctor::run(&engine, &cfg_path).await;
            if fails > 0 {
                bail!("{fails} check(s) failed");
            }
            Ok(())
        }
        Cmd::Ask { agent, skill, cwd, prompt } => {
            print_notices(&engine);
            let cwd = cwd.map(std::path::absolute).transpose()?;
            let prompt = prompt.join(" ");
            let sid = engine.new_session(Some(&agent), &agents::one_line(&prompt, 40), cwd)?;
            let out = engine.turn(&sid, TurnInput { prompt, role: layers::Role::Chat, skill }, None).await?;
            println!("{}", out.text);
            eprintln!("(session {sid})");
            Ok(())
        }
        Cmd::Reply { session, prompt } => {
            print_notices(&engine);
            engine.store.session(&session)?.with_context(|| format!("no session '{session}'"))?;
            let out = engine.turn(&session, TurnInput { prompt: prompt.join(" "), role: layers::Role::Chat, skill: None }, None).await?;
            println!("{}", out.text);
            Ok(())
        }
        Cmd::Plan { yes, per_provider, cwd, goal } => {
            start_notifier(&engine, false).await;
            print_notices(&engine);
            let cwd = cwd.map(std::path::absolute).transpose()?;
            let id = plan::create(&engine, &goal.join(" "), cwd, per_provider)?;
            eprintln!("drafting plan {id}…");
            let p = wait_plan(&engine, &id, |p| p.status != plan::PlanStatus::Drafting).await?;
            for n in &p.notes {
                eprintln!("note: {n}");
            }
            if p.status != plan::PlanStatus::AwaitingApproval {
                bail!("planning failed");
            }
            println!("{}", plan::summary(&p));
            if !yes {
                eprintln!("approve with: senapati approve {id}");
                return Ok(());
            }
            run_plan(&engine, &id).await
        }
        Cmd::Approve { id } => {
            start_notifier(&engine, false).await;
            print_notices(&engine);
            run_plan(&engine, &id).await
        }
        Cmd::Serve => {
            print_notices(&engine);
            let n = start_notifier(&engine, true).await;
            if n.channels().is_empty() {
                bail!("no alert channel is enabled and configured");
            }
            eprintln!("serving: {} (Ctrl+C to stop)", n.channels().join(", "));
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Cmd::Whatsapp { op: WhatsappOp::Link } => {
            eprintln!("WARNING: this links an unofficial WhatsApp client. WhatsApp can ban the linked number.");
            eprintln!("Link a spare number, never your main one. Session keys are saved in {}.", engine.home.join("whatsapp-auth").display());
            notify::whatsapp_link(&engine)
        }
        Cmd::NotifyTest => {
            print_notices(&engine);
            let n = start_notifier(&engine, false).await;
            if n.channels().is_empty() {
                bail!("no alert channel is enabled and configured");
            }
            // The WhatsApp sidecar needs a moment to connect.
            if n.channels().contains(&"whatsapp") {
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            }
            n.send("test alert ✅").await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            eprintln!("sent to: {}", n.channels().join(", "));
            Ok(())
        }
        Cmd::Skills => {
            for s in engine.skills(&engine.cwd()) {
                println!("{:<28} {}\n{:<28} {}", s.name, agents::one_line(&s.description, 90), "", s.dir.display());
            }
            Ok(())
        }
        Cmd::Init { .. } | Cmd::Secret { .. } => unreachable!(),
    }
}

async fn start_notifier(engine: &Arc<Engine>, listen: bool) -> Arc<notify::Notifier> {
    let n = notify::Notifier::start(engine, listen).await;
    let _ = engine.notifier.set(n.clone());
    n
}

/// Headless commands: show engine notices on stderr.
fn print_notices(engine: &Arc<Engine>) {
    let mut rx = engine.events.subscribe();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if let engine::AppEvent::Notice(s) = ev {
                eprintln!("· {s}");
            }
        }
    });
}

async fn wait_plan(engine: &Engine, id: &str, mut done: impl FnMut(&plan::Plan) -> bool) -> Result<plan::Plan> {
    loop {
        let p = plan::load(engine, id)?;
        if done(&p) {
            return Ok(p);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

async fn run_plan(engine: &Arc<Engine>, id: &str) -> Result<()> {
    plan::approve(engine, id)?;
    let mut last = String::new();
    let p = wait_plan(engine, id, |p| {
        let line: String = p
            .steps
            .iter()
            .map(|s| {
                let queued = s.status == plan::StepStatus::Running
                    && s.session_id.as_ref().and_then(|sid| engine.store.session(sid).ok().flatten()).is_some_and(|x| x.status == "queued");
                if queued { format!("{}:Queued ", s.id) } else { format!("{}:{:?} ", s.id, s.status) }
            })
            .collect();
        if line != last {
            eprintln!("{line}");
            last = line;
        }
        p.finished()
    })
    .await?;
    for s in &p.steps {
        println!("── {} [{}] {:?}: {}", s.id, s.agent, s.status, s.title);
        if let Some(h) = &s.handoff {
            println!("{h}");
        }
        if let Some(e) = &s.error {
            println!("error: {e}");
        }
    }
    // Let the final alert go out before the process exits.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    if p.status != plan::PlanStatus::Done {
        bail!("plan {id} ended {:?}", p.status);
    }
    Ok(())
}

fn init(home: &Path, skills: Option<PathBuf>) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let cfg = home.join("senapati.toml");
    if cfg.exists() {
        println!("config exists, left unchanged: {}", cfg.display());
    } else {
        std::fs::write(&cfg, config::EXAMPLE_CONFIG)?;
        println!("wrote {}", cfg.display());
    }
    let src = skills.or_else(|| Some(PathBuf::from("skills")).filter(|p| p.is_dir()));
    if let Some(src) = src {
        let dst = home.join("skills");
        let mut copied = 0;
        for entry in std::fs::read_dir(&src).with_context(|| format!("read {}", src.display()))? {
            let p = entry?.path();
            let target = dst.join(p.file_name().unwrap());
            if p.join("SKILL.md").exists() && !target.exists() {
                copy_dir(&p, &target)?;
                copied += 1;
            }
        }
        println!("copied {copied} new skill(s) to {}", dst.display());
    }
    println!("next: senapati doctor");
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let p = e?.path();
        let t = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &t)?;
        } else {
            std::fs::copy(&p, &t)?;
        }
    }
    Ok(())
}

fn secret(op: &SecretOp) -> Result<()> {
    match op {
        SecretOp::Set { name } => {
            let v = rpassword::prompt_password(format!("{name}: "))?;
            if v.trim().is_empty() {
                bail!("empty value");
            }
            secrets::set(name, v.trim())?;
            println!("stored {name} in Windows Credential Manager");
        }
        SecretOp::Delete { name } => {
            secrets::delete(name)?;
            println!("deleted {name}");
        }
        SecretOp::List => {
            for (name, what) in secrets::KNOWN {
                println!("{:<22} {:<4} {what}", name, if secrets::is_set(name) { "set" } else { "-" });
            }
        }
    }
    Ok(())
}
