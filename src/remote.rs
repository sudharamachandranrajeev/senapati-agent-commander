//! Commands sent from allowed Telegram chats or WhatsApp numbers.
//! Read-only commands always work; the rest need `notify.remote_control = true`.

use crate::engine::{Engine, TurnInput};
use crate::layers::Role;
use crate::plan::{self, PlanStatus};
use std::sync::Arc;

const HELP: &str = "/status · /plans · /plan <goal> · /approve <plan> · /cancel <plan> · \
/ask [agent] <prompt> · /reply <session> <text> · /stop <session>";

pub async fn handle(engine: &Arc<Engine>, text: &str) -> String {
    let text = text.trim();
    let (cmd, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let rest = rest.trim();
    match cmd {
        "/help" | "/start" => return HELP.into(),
        "/status" => return status(engine),
        "/plans" => return plans(engine),
        _ => {}
    }
    if !engine.cfg.notify.remote_control {
        return "remote control is off (notify.remote_control = false). Read-only: /status, /plans".into();
    }
    let r: anyhow::Result<String> = match cmd {
        "/plan" if !rest.is_empty() => plan::create(engine, rest, None, None).map(|id| format!("drafting plan {id}…")),
        "/approve" => plan::approve(engine, rest).map(|_| format!("plan {rest} running")),
        "/cancel" => plan::cancel(engine, rest).map(|_| format!("plan {rest} cancelled")),
        "/ask" if !rest.is_empty() => {
            let (agent, prompt) = match rest.split_once(char::is_whitespace) {
                Some((a, p)) if engine.cfg.agent(a).is_some() => (Some(a), p),
                _ => (None, rest),
            };
            let title = crate::agents::one_line(prompt, 40);
            engine.new_session(agent, &title, None).map(|sid| {
                reply_later(engine, sid.clone(), prompt.to_string());
                format!("session {sid} started")
            })
        }
        "/reply" => match rest.split_once(char::is_whitespace) {
            Some((sid, msg)) if engine.store.session(sid).ok().flatten().is_some() => {
                reply_later(engine, sid.to_string(), msg.to_string());
                Ok(format!("sent to {sid}"))
            }
            _ => Err(anyhow::anyhow!("usage: /reply <session> <text>")),
        },
        "/stop" => Ok(if engine.stop(rest) { format!("stopped {rest}") } else { format!("{rest} is not running") }),
        _ => Ok(format!("unknown command. {HELP}")),
    };
    r.unwrap_or_else(|e| format!("error: {e:#}"))
}

/// Run the turn, then push the answer to every channel.
fn reply_later(engine: &Arc<Engine>, sid: String, prompt: String) {
    let e = engine.clone();
    tokio::spawn(async move {
        let msg = match e.turn(&sid, TurnInput { prompt, role: Role::Chat, skill: None }, None).await {
            Ok(o) => format!("💬 [{sid}]\n{}", crate::agents::tail(&o.text, 3500)),
            Err(err) => format!("❌ [{sid}] {err:#}"),
        };
        if let Some(n) = e.notifier.get() {
            n.send(&msg).await;
        }
    });
}

fn status(engine: &Engine) -> String {
    let mut s = String::new();
    for (p, running, max) in engine.provider_load() {
        s.push_str(&format!("{p}: {running}/{max} busy\n"));
    }
    let busy: Vec<_> = engine
        .store
        .sessions()
        .unwrap_or_default()
        .into_iter()
        .filter(|x| x.status == "running" || x.status == "queued")
        .map(|x| format!("{} [{}] {} – {}", x.id, x.agent, x.status, x.title))
        .collect();
    if busy.is_empty() {
        s.push_str("no sessions running");
    } else {
        s.push_str(&busy.join("\n"));
    }
    s
}

fn plans(engine: &Engine) -> String {
    let v: Vec<_> = plan::all(engine)
        .into_iter()
        .take(5)
        .map(|p| {
            let done = p.steps.iter().filter(|s| s.status == plan::StepStatus::Done).count();
            let flag = if p.status == PlanStatus::AwaitingApproval { " ← /approve" } else { "" };
            format!("{} {:?} {done}/{} {}{flag}", p.id, p.status, p.steps.len(), crate::agents::one_line(&p.goal, 60))
        })
        .collect();
    if v.is_empty() { "no plans".into() } else { v.join("\n") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(remote: bool) -> Arc<Engine> {
        let mut cfg = crate::config::Config::parse(crate::config::EXAMPLE_CONFIG).unwrap();
        cfg.notify.remote_control = remote;
        Engine::new(cfg, std::env::temp_dir(), crate::store::Store::memory().unwrap())
    }

    #[tokio::test]
    async fn read_only_commands_work_when_remote_control_is_off() {
        let e = engine(false);
        assert!(handle(&e, "/status").await.contains("anthropic: 0/1 busy"));
        assert_eq!(handle(&e, "/plans").await, "no plans");
        let r = handle(&e, "/ask claude delete everything").await;
        assert!(r.contains("remote control is off"), "{r}");
        assert!(e.store.sessions().unwrap().is_empty(), "no session may start");
        assert!(handle(&e, "/approve p1").await.contains("remote control is off"));
    }

    #[tokio::test]
    async fn control_commands_validate_input() {
        let e = engine(true);
        assert!(handle(&e, "/approve nope").await.starts_with("error: no plan 'nope'"));
        assert!(handle(&e, "/reply zzz hi").await.contains("usage"));
        assert!(handle(&e, "/stop zzz").await.contains("not running"));
        assert!(handle(&e, "hello").await.contains("unknown command"));
    }
}
