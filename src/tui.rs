//! Terminal UI: Sessions, Plans and Agents tabs over the engine.

use crate::config::CavemanMode;
use crate::engine::{AppEvent, Engine, TurnInput};
use crate::layers::Role;
use crate::plan::{self, Plan, PlanStatus, StepStatus};
use crate::store::{Message, Session};
use anyhow::{anyhow, bail, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures_util::StreamExt;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Sessions,
    Plans,
    Agents,
}

const TABS: [Tab; 3] = [Tab::Sessions, Tab::Plans, Tab::Agents];

const HELP: &str = "/new [agent] [title] · /agent <name> · /skill <name> <prompt> · /skills · \
/plan [--per-provider N] <goal> · /approve · /edit · /cancel · /stop · /cwd <path> · \
/caveman off|lite|full|ultra|auto · /quit";

struct App {
    engine: Arc<Engine>,
    tab: Tab,
    sessions: Vec<Session>,
    sel_session: usize,
    messages: Vec<Message>,
    plans: Vec<Plan>,
    sel_plan: usize,
    input: String,
    cursor: usize,
    scroll_up: u16,
    notices: VecDeque<String>,
    quit: bool,
    edit_plan: Option<String>,
}

pub async fn run(engine: Arc<Engine>) -> Result<()> {
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        default_hook(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let result = event_loop(&mut terminal, engine).await;
    restore();
    result
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = std::io::stdout().execute(LeaveAlternateScreen);
}

async fn event_loop(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, engine: Arc<Engine>) -> Result<()> {
    let mut rx = engine.events.subscribe();
    let mut app = App {
        engine,
        tab: Tab::Sessions,
        sessions: vec![],
        sel_session: 0,
        messages: vec![],
        plans: vec![],
        sel_plan: 0,
        input: String::new(),
        cursor: 0,
        scroll_up: 0,
        notices: VecDeque::new(),
        quit: false,
        edit_plan: None,
    };
    app.note("Senapati ready. Type a message, /plan <goal>, or /help.");
    app.reload();
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        terminal.draw(|f| draw(f, &app))?;
        let mut dirty = false;
        tokio::select! {
            k = keys.next() => match k {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => app.on_key(key),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            ev = rx.recv() => dirty |= app.on_event(ev),
            _ = tick.tick() => {}
        }
        while let Ok(ev) = rx.try_recv() {
            dirty |= app.on_event(Ok(ev));
        }
        if dirty {
            app.reload();
        }
        if let Some(id) = app.edit_plan.take() {
            restore();
            let r = edit_plan(&app.engine, &id);
            enable_raw_mode()?;
            std::io::stdout().execute(EnterAlternateScreen)?;
            terminal.clear()?;
            match r {
                Ok(()) => app.note(format!("plan {id} updated")),
                Err(e) => app.note(format!("edit failed: {e:#}")),
            }
            app.reload();
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn edit_plan(engine: &Engine, id: &str) -> Result<()> {
    let p = plan::load(engine, id)?;
    if p.status != PlanStatus::AwaitingApproval {
        bail!("only a plan awaiting approval can be edited");
    }
    let dir = engine.home.join("plans");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{id}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&p)?)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "notepad".into());
    let mut parts = editor.split_whitespace();
    let prog = parts.next().ok_or_else(|| anyhow!("EDITOR is empty"))?;
    let status = std::process::Command::new(prog).args(parts).arg(&path).status()?;
    if !status.success() {
        bail!("editor exited with {status}");
    }
    plan::import_edit(engine, id, &std::fs::read_to_string(&path)?)
}

impl App {
    fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        let t = chrono::Local::now().format("%H:%M:%S");
        self.notices.push_back(format!("{t} {s}"));
        while self.notices.len() > 100 {
            self.notices.pop_front();
        }
    }

    fn on_event(&mut self, ev: std::result::Result<AppEvent, tokio::sync::broadcast::error::RecvError>) -> bool {
        match ev {
            Ok(AppEvent::Notice(s)) => {
                self.note(s);
                false
            }
            Ok(_) | Err(_) => true,
        }
    }

    fn selected_session(&self) -> Option<&Session> {
        self.sessions.get(self.sel_session)
    }

    fn selected_plan(&self) -> Option<&Plan> {
        self.plans.get(self.sel_plan)
    }

    fn reload(&mut self) {
        let keep = self.selected_session().map(|s| s.id.clone());
        self.sessions = self.engine.store.sessions().unwrap_or_default();
        if let Some(k) = keep {
            if let Some(i) = self.sessions.iter().position(|s| s.id == k) {
                self.sel_session = i;
            }
        }
        self.sel_session = self.sel_session.min(self.sessions.len().saturating_sub(1));
        self.messages = match self.selected_session() {
            Some(s) => self.engine.store.messages(&s.id).unwrap_or_default(),
            None => vec![],
        };
        let keep = self.selected_plan().map(|p| p.id.clone());
        self.plans = plan::all(&self.engine);
        if let Some(k) = keep {
            if let Some(i) = self.plans.iter().position(|p| p.id == k) {
                self.sel_plan = i;
            }
        }
        self.sel_plan = self.sel_plan.min(self.plans.len().saturating_sub(1));
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('q') if ctrl => self.quit = true,
            KeyCode::Char('n') if ctrl => self.run_command("/new"),
            KeyCode::Char('y') if ctrl => self.run_command("/approve"),
            KeyCode::Char('e') if ctrl => self.run_command("/edit"),
            KeyCode::Char('x') if ctrl => {
                let cmd = if self.tab == Tab::Plans { "/cancel" } else { "/stop" };
                self.run_command(cmd)
            }
            KeyCode::Tab => self.tab = TABS[(TABS.iter().position(|t| *t == self.tab).unwrap() + 1) % 3],
            KeyCode::BackTab => self.tab = TABS[(TABS.iter().position(|t| *t == self.tab).unwrap() + 2) % 3],
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.scroll_up = self.scroll_up.saturating_add(10),
            KeyCode::PageDown => self.scroll_up = self.scroll_up.saturating_sub(10),
            KeyCode::End if ctrl => self.scroll_up = 0,
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input).trim().to_string();
                self.cursor = 0;
                if !text.is_empty() {
                    self.submit(&text);
                }
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let i = self.byte_at(self.cursor);
                self.input.remove(i);
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                let i = self.byte_at(self.cursor);
                self.input.remove(i);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) if !ctrl => {
                let i = self.byte_at(self.cursor);
                self.input.insert(i, c);
                self.cursor += 1;
            }
            _ => {}
        }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.input.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.input.len())
    }

    fn move_sel(&mut self, d: i32) {
        let (sel, len) = match self.tab {
            Tab::Sessions => (&mut self.sel_session, self.sessions.len()),
            Tab::Plans => (&mut self.sel_plan, self.plans.len()),
            Tab::Agents => return,
        };
        if len == 0 {
            return;
        }
        *sel = (*sel as i32 + d).clamp(0, len as i32 - 1) as usize;
        self.scroll_up = 0;
        self.reload();
    }

    fn submit(&mut self, text: &str) {
        if text.starts_with('/') {
            self.run_command(text);
        } else if self.tab == Tab::Plans {
            self.run_command(&format!("/plan {text}"));
        } else {
            self.send(text, None);
        }
    }

    fn send(&mut self, text: &str, skill: Option<String>) {
        let sid = match self.selected_session() {
            Some(s) => s.id.clone(),
            None => match self.engine.new_session(None, &crate::agents::one_line(text, 40), None) {
                Ok(id) => id,
                Err(e) => return self.note(format!("{e:#}")),
            },
        };
        self.tab = Tab::Sessions;
        self.scroll_up = 0;
        if let Err(e) = self.engine.send(&sid, TurnInput { prompt: text.to_string(), role: Role::Chat, skill }) {
            self.note(format!("{e:#}"));
        }
        self.reload();
        if let Some(i) = self.sessions.iter().position(|s| s.id == sid) {
            self.sel_session = i;
            self.reload();
        }
    }

    fn run_command(&mut self, line: &str) {
        if let Err(e) = self.command(line) {
            self.note(format!("{e:#}"));
        }
    }

    fn command(&mut self, line: &str) -> Result<()> {
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or_default();
        let rest: Vec<&str> = words.collect();
        let engine = self.engine.clone();
        let plan_id = |rest: &[&str], app: &App| -> Result<String> {
            rest.first()
                .map(|s| s.to_string())
                .or_else(|| app.selected_plan().map(|p| p.id.clone()))
                .ok_or_else(|| anyhow!("no plan selected"))
        };
        match cmd {
            "/help" => self.note(HELP),
            "/quit" | "/exit" => self.quit = true,
            "/new" => {
                let (agent, title) = match rest.first() {
                    Some(a) if engine.cfg.agent(a).is_some() => (Some(*a), rest[1..].join(" ")),
                    _ => (None, rest.join(" ")),
                };
                let title = if title.is_empty() { "chat".to_string() } else { title };
                let id = engine.new_session(agent, &title, None)?;
                self.tab = Tab::Sessions;
                self.reload();
                self.sel_session = self.sessions.iter().position(|s| s.id == id).unwrap_or(0);
                self.reload();
            }
            "/agent" => {
                let name = rest.first().ok_or_else(|| anyhow!("usage: /agent <name>"))?;
                engine.cfg.agent(name).ok_or_else(|| anyhow!("no enabled agent '{name}'"))?;
                let s = self.selected_session().ok_or_else(|| anyhow!("no session selected"))?;
                engine.store.set_session_agent(&s.id, name)?;
                self.note(format!("session {} now uses {name} (fresh vendor context)", s.id));
                self.reload();
            }
            "/skill" => {
                let name = rest.first().ok_or_else(|| anyhow!("usage: /skill <name> <prompt>"))?;
                if rest.len() < 2 {
                    bail!("usage: /skill <name> <prompt>");
                }
                self.send(&rest[1..].join(" "), Some(name.to_string()));
            }
            "/skills" => {
                let cwd = self.selected_session().map(|s| PathBuf::from(&s.cwd)).unwrap_or_else(|| engine.cwd());
                let names: Vec<String> = engine.skills(&cwd).into_iter().map(|s| s.name).collect();
                self.note(format!("{} skills: {}", names.len(), names.join(", ")));
            }
            "/plan" => {
                let mut per = None;
                let mut goal = rest.clone();
                if goal.first() == Some(&"--per-provider") {
                    per = Some(goal.get(1).and_then(|n| n.parse::<usize>().ok()).ok_or_else(|| anyhow!("--per-provider needs a number"))?);
                    goal.drain(..2);
                }
                if goal.is_empty() {
                    bail!("usage: /plan [--per-provider N] <goal>");
                }
                let id = plan::create(&engine, &goal.join(" "), None, per)?;
                self.tab = Tab::Plans;
                self.reload();
                self.sel_plan = self.plans.iter().position(|p| p.id == id).unwrap_or(0);
                self.note(format!("drafting plan {id}…"));
            }
            "/approve" | "/run" => {
                let id = plan_id(&rest, self)?;
                plan::approve(&engine, &id)?;
                self.note(format!("plan {id} running"));
            }
            "/cancel" => {
                let id = plan_id(&rest, self)?;
                plan::cancel(&engine, &id)?;
                self.note(format!("plan {id} cancelled"));
            }
            "/edit" => self.edit_plan = Some(plan_id(&rest, self)?),
            "/stop" => {
                let s = self.selected_session().ok_or_else(|| anyhow!("no session selected"))?;
                let id = s.id.clone();
                if !engine.stop(&id) {
                    self.note(format!("{id} is not running"));
                }
            }
            "/cwd" => {
                let p = PathBuf::from(rest.join(" "));
                if !p.is_dir() {
                    bail!("not a folder: {}", p.display());
                }
                *engine.default_cwd.write().unwrap() = std::path::absolute(&p)?;
                self.note(format!("new sessions start in {}", engine.cwd().display()));
            }
            "/caveman" => {
                let m: CavemanMode = rest.first().copied().unwrap_or("").parse()?;
                engine.set_caveman(m);
                self.note(format!("caveman: {m:?}"));
            }
            _ => bail!("unknown command {cmd}. {HELP}"),
        }
        Ok(())
    }
}

fn status_style(status: &str) -> (&'static str, Color) {
    match status {
        "running" => ("●", Color::Green),
        "queued" => ("◌", Color::Yellow),
        "error" => ("✖", Color::Red),
        _ => ("○", Color::DarkGray),
    }
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(3), Constraint::Length(1)]).split(area);

    let titles = ["Sessions", "Plans", "Agents"];
    let idx = TABS.iter().position(|t| *t == app.tab).unwrap();
    let header = Layout::horizontal([Constraint::Length(30), Constraint::Min(10)]).split(rows[0]);
    f.render_widget(Paragraph::new(Span::styled(" Senapati · Agent-Commander ", Style::new().bold().fg(Color::Black).bg(Color::Yellow))), header[0]);
    let right = format!("caveman:{:?}  cwd:{}", app.engine.caveman(), app.engine.cwd().display()).to_lowercase();
    let tabs_area = Layout::horizontal([Constraint::Length(32), Constraint::Min(0)]).split(header[1]);
    f.render_widget(Tabs::new(titles).select(idx).highlight_style(Style::new().bold().fg(Color::Yellow)), tabs_area[0]);
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right).fg(Color::DarkGray), tabs_area[1]);

    match app.tab {
        Tab::Sessions => draw_sessions(f, app, rows[1]),
        Tab::Plans => draw_plans(f, app, rows[1]),
        Tab::Agents => draw_agents(f, app, rows[1]),
    }

    let target = match app.tab {
        Tab::Sessions => app.selected_session().map(|s| format!(" › {} [{}] ", s.id, s.agent)).unwrap_or(" › new chat ".into()),
        Tab::Plans => " › new plan goal (or /command) ".into(),
        Tab::Agents => " › /command ".into(),
    };
    let input = Paragraph::new(app.input.as_str()).block(Block::default().borders(Borders::ALL).title(target));
    f.render_widget(input, rows[2]);
    let cx = rows[2].x + 1 + app.input.chars().take(app.cursor).map(|c| if c.is_ascii() { 1 } else { 2 }).sum::<u16>();
    f.set_cursor_position((cx.min(rows[2].right().saturating_sub(2)), rows[2].y + 1));

    let hint = "Tab views · ↑↓ select · PgUp/PgDn scroll · Ctrl+N new · Ctrl+Y approve · Ctrl+E edit · Ctrl+X stop/cancel · Ctrl+Q quit";
    let last = app.notices.back().cloned().unwrap_or_default();
    let status = if last.is_empty() { hint.to_string() } else { format!("{last}   │ {hint}") };
    f.render_widget(Paragraph::new(status).fg(Color::DarkGray), rows[3]);
}

fn list_area(area: Rect) -> (Option<Rect>, Rect) {
    if area.width < 70 {
        return (None, area);
    }
    let cols = Layout::horizontal([Constraint::Length(34), Constraint::Min(20)]).split(area);
    (Some(cols[0]), cols[1])
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let (left, right) = list_area(area);
    if let Some(left) = left {
        let items: Vec<ListItem> = app
            .sessions
            .iter()
            .map(|s| {
                let (icon, color) = status_style(&s.status);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{icon} "), Style::new().fg(color)),
                    Span::styled(format!("{} ", s.agent), Style::new().fg(Color::Cyan)),
                    Span::raw(s.title.clone()),
                ]))
            })
            .collect();
        let mut st = ListState::default().with_selected(if app.sessions.is_empty() { None } else { Some(app.sel_session) });
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" sessions "))
            .highlight_style(Style::new().bg(Color::DarkGray));
        f.render_stateful_widget(list, left, &mut st);
    }

    let mut lines: Vec<Line> = Vec::new();
    for m in &app.messages {
        let (prefix, style) = match m.role.as_str() {
            "user" => ("› ", Style::new().fg(Color::Cyan).bold()),
            "tool" => ("  ⚙ ", Style::new().fg(Color::DarkGray)),
            "system" => ("• ", Style::new().fg(Color::Yellow)),
            "error" => ("✖ ", Style::new().fg(Color::Red)),
            _ => ("", Style::new()),
        };
        if m.role == "user" && !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        for (i, l) in m.content.lines().enumerate() {
            let p = if i == 0 { prefix } else { if prefix.is_empty() { "" } else { "  " } };
            lines.push(Line::from(Span::styled(format!("{p}{l}"), style)));
        }
    }
    let title = match app.selected_session() {
        Some(s) => format!(" {} · {} · {} · {} ", s.id, s.agent, s.status, s.cwd),
        None => " no session: type to start one ".into(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let inner_w = right.width.saturating_sub(2);
    let inner_h = right.height.saturating_sub(2) as usize;
    let total = para.line_count(inner_w);
    let max_scroll = total.saturating_sub(inner_h);
    let offset = max_scroll.saturating_sub(app.scroll_up as usize);
    f.render_widget(para.block(block).scroll((offset.min(u16::MAX as usize) as u16, 0)), right);
}

fn step_icon(s: StepStatus) -> (&'static str, Color) {
    match s {
        StepStatus::Pending => ("○", Color::DarkGray),
        StepStatus::Running => ("●", Color::Green),
        StepStatus::Done => ("✔", Color::Green),
        StepStatus::Failed => ("✖", Color::Red),
        StepStatus::Skipped => ("–", Color::DarkGray),
    }
}

fn draw_plans(f: &mut Frame, app: &App, area: Rect) {
    let (left, right) = list_area(area);
    if let Some(left) = left {
        let items: Vec<ListItem> = app
            .plans
            .iter()
            .map(|p| {
                let done = p.steps.iter().filter(|s| s.status == StepStatus::Done).count();
                let color = match p.status {
                    PlanStatus::AwaitingApproval => Color::Yellow,
                    PlanStatus::Running | PlanStatus::Drafting => Color::Green,
                    PlanStatus::Failed => Color::Red,
                    _ => Color::DarkGray,
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} {:?} ", p.id, p.status), Style::new().fg(color)),
                    Span::raw(format!("{done}/{} {}", p.steps.len(), p.goal)),
                ]))
            })
            .collect();
        let mut st = ListState::default().with_selected(if app.plans.is_empty() { None } else { Some(app.sel_plan) });
        let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" plans ")).highlight_style(Style::new().bg(Color::DarkGray));
        f.render_stateful_widget(list, left, &mut st);
    }
    let Some(p) = app.selected_plan() else {
        f.render_widget(
            Paragraph::new("No plans. Type a goal below and press Enter; a planner agent drafts the steps for approval.")
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL)),
            right,
        );
        return;
    };
    let mut lines = vec![
        Line::from(vec![Span::styled("Goal: ", Style::new().bold()), Span::raw(p.goal.clone())]),
        Line::from(format!(
            "Status: {:?} · per provider: {} · cwd: {}",
            p.status,
            p.per_provider.map(|n| n.to_string()).unwrap_or("config".into()),
            p.cwd
        )),
    ];
    for n in &p.notes {
        lines.push(Line::styled(format!("note: {n}"), Style::new().fg(Color::Yellow)));
    }
    if p.status == PlanStatus::AwaitingApproval {
        lines.push(Line::styled("Ctrl+Y approve · Ctrl+E edit in $EDITOR · Ctrl+X cancel", Style::new().fg(Color::Yellow).bold()));
    }
    lines.push(Line::raw(""));
    for s in &p.steps {
        let waiting = s.status == StepStatus::Running
            && s.session_id.as_ref().is_some_and(|sid| app.sessions.iter().any(|x| &x.id == sid && x.status == "queued"));
        let (icon, color) = if waiting { ("◌", Color::Yellow) } else { step_icon(s.status) };
        let deps = if s.depends_on.is_empty() { String::new() } else { format!(" ← {}", s.depends_on.join(",")) };
        let skill = s.skill.as_ref().map(|k| format!(" +{k}")).unwrap_or_default();
        let sess = s.session_id.as_ref().map(|k| format!(" ({k})")).unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("{icon} {} ", s.id), Style::new().fg(color)),
            Span::styled(format!("[{}{skill}] ", s.agent), Style::new().fg(Color::Cyan)),
            Span::raw(format!("{}{deps}{sess}", s.title)),
        ]));
        if let Some(h) = &s.handoff {
            lines.push(Line::styled(format!("    ↳ {}", crate::agents::one_line(h, 400)), Style::new().fg(Color::DarkGray)));
        }
        if let Some(e) = &s.error {
            lines.push(Line::styled(format!("    ✖ {}", crate::agents::one_line(e, 400)), Style::new().fg(Color::Red)));
        }
        if p.status == PlanStatus::AwaitingApproval {
            lines.push(Line::styled(format!("    {}", crate::agents::one_line(&s.prompt, 300)), Style::new().fg(Color::DarkGray)));
        }
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).title(format!(" plan {} ", p.id)));
    f.render_widget(para, right);
}

fn draw_agents(f: &mut Frame, app: &App, area: Rect) {
    let e = &app.engine;
    let mut lines = vec![Line::styled("Agents", Style::new().bold())];
    for a in &e.cfg.agents {
        let model = a.model.clone().unwrap_or_default();
        let state = if a.enabled { "" } else { "  (disabled)" };
        let fb = if a.fallback.is_empty() { String::new() } else { format!("  fallback→{}", a.fallback.join(",")) };
        let mcp = if a.mcp.is_empty() { String::new() } else { format!("  mcp:{}", a.mcp.join(",")) };
        lines.push(Line::from(format!("  {:<14} {:<14} {:<10} {model}{mcp}{fb}{state}", a.name, format!("{:?}", a.kind), a.provider)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Providers (busy / max at once)", Style::new().bold()));
    for (p, busy, max) in e.provider_load() {
        lines.push(Line::from(format!("  {p:<12} {busy}/{max}")));
    }
    lines.push(Line::raw(""));
    let skills: Vec<String> = e.skills(&e.cwd()).into_iter().map(|s| s.name).collect();
    lines.push(Line::styled(format!("Skills ({})", skills.len()), Style::new().bold()));
    lines.push(Line::from(format!("  {}", skills.join(", "))));
    lines.push(Line::raw(""));
    lines.push(Line::styled("MCP servers", Style::new().bold()));
    lines.push(Line::from(format!("  {}", e.cfg.mcp.keys().cloned().collect::<Vec<_>>().join(", "))));
    lines.push(Line::raw(""));
    let ch = e.notifier.get().map(|n| n.channels().join(", ")).unwrap_or_default();
    lines.push(Line::styled("Alerts", Style::new().bold()));
    lines.push(Line::from(format!(
        "  channels: {}  events: {}  remote control: {}",
        if ch.is_empty() { "none".into() } else { ch },
        e.cfg.notify.events.join(","),
        if e.cfg.notify.remote_control { "on" } else { "off" }
    )));
    lines.push(Line::raw(""));
    lines.push(Line::styled("Recent notices", Style::new().bold()));
    for n in app.notices.iter().rev().take(15) {
        lines.push(Line::styled(format!("  {n}"), Style::new().fg(Color::DarkGray)));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let cfg = crate::config::Config::parse(crate::config::EXAMPLE_CONFIG).unwrap();
        let engine = Engine::new(cfg, std::env::temp_dir(), crate::store::Store::memory().unwrap());
        let sid = engine.new_session(Some("claude"), "demo chat", None).unwrap();
        engine.store.add_message(&sid, "user", "hello there").unwrap();
        engine.store.add_message(&sid, "tool", "Bash ls").unwrap();
        engine.store.add_message(&sid, "assistant", "General Kenobi").unwrap();
        let p = Plan {
            id: "p1".into(),
            goal: "ship it".into(),
            cwd: ".".into(),
            status: PlanStatus::AwaitingApproval,
            per_provider: None,
            notes: vec!["s2: unknown skill 'x' dropped".into()],
            steps: crate::plan::parse_steps(r#"{"steps":[{"id":"s1","title":"write code","agent":"claude","prompt":"do it"}]}"#).unwrap(),
        };
        engine.store.save_plan("p1", &serde_json::to_string(&p).unwrap()).unwrap();
        let mut a = App {
            engine,
            tab: Tab::Sessions,
            sessions: vec![],
            sel_session: 0,
            messages: vec![],
            plans: vec![],
            sel_plan: 0,
            input: "draft ✓".into(),
            cursor: 7,
            scroll_up: 0,
            notices: VecDeque::new(),
            quit: false,
            edit_plan: None,
        };
        a.reload();
        a
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn renders_all_tabs_wide_and_narrow() {
        let mut a = app();
        for (w, h) in [(120, 30), (60, 20)] {
            a.tab = Tab::Sessions;
            let s = render(&a, w, h);
            assert!(s.contains("General Kenobi"), "transcript at {w}x{h}");
            assert!(s.contains("Agent-Commander") || w < 80);
            a.tab = Tab::Plans;
            let s = render(&a, w, h);
            assert!(s.contains("ship it"), "plan at {w}x{h}");
            a.tab = Tab::Agents;
            assert!(render(&a, w, h).contains("qwen-coder"));
        }
        assert!(render(&{ a.tab = Tab::Sessions; a }, 120, 30).contains("demo chat"), "session list when wide");
    }

    #[test]
    fn commands_parse_without_running_agents() {
        let mut a = app();
        a.run_command("/caveman ultra");
        assert_eq!(a.engine.caveman(), CavemanMode::Ultra);
        a.run_command("/new codex review");
        assert_eq!(a.selected_session().unwrap().agent, "codex");
        a.run_command("/agent nope");
        assert!(a.notices.back().unwrap().contains("no enabled agent"));
        a.run_command("/bogus");
        assert!(a.notices.back().unwrap().contains("unknown command"));
    }
}
