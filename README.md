# Senapati (Agent-Commander)

A Windows terminal for running several coding agents and local models from one place, in the
spirit of [herder](https://github.com/herder-sh/herder), which only runs on Linux and macOS.

- **Agents:** Claude Code and Codex, driven through their own CLIs. Senapati never touches
  their login tokens. It also drives any OpenAI-compatible endpoint (LM Studio, Ollama,
  OpenRouter) and any CLI that reads a prompt on stdin.
- **Delegation plans:** a planner agent splits a goal into steps. You approve or edit the
  plan, then steps run as soon as their dependencies finish. Each step passes a short handoff
  to the steps that depend on it.
- **One agent per provider:** at most one Claude, one OpenAI and one local agent run at the
  same time. Steps for different providers run in parallel. Raise the limit per provider in
  config or per plan with `--per-provider N`.
- **Failover:** when an agent hits a usage or rate limit, the turn moves to its `fallback`
  agent, with the recent conversation replayed.
- **Skills:** `SKILL.md` folders (the vendored `replica-*` set, your `~/.claude/skills`, and
  project `.claude/skills`) work with every provider. Claude loads skills natively; the other
  agents get the skill text inline.
- **MCP for everyone:** servers such as REA and caveman are configured once. Claude gets them
  through `--mcp-config` and codex through `-c mcp_servers.*`. Local models reach them through
  a built-in MCP bridge, including models that write tool calls as plain JSON text.
- **Caveman layer:** a terse-output system prompt for every provider (`off`, `lite`, `full`,
  `ultra` or `auto`). `auto` uses `full` for chat and plan steps and `ultra` for the planner.
- **Alerts and remote control:** Telegram (two-way), Discord webhook (one-way), and WhatsApp
  through a linked bot account (two-way).

## Build

Requires Rust (MSVC toolchain). For WhatsApp you also need Node 20 or newer.

```powershell
$env:CARGO_HOME = "$PWD\.tools\cargo-home"   # optional: keep the crate cache in the project
cargo build --release
# WhatsApp only:
cd sidecars\whatsapp; npm ci --cache ..\..\.tools\npm-cache; cd ..\..
```

The binary is `target\release\senapati.exe`.

## First run

```powershell
senapati init      # writes %LOCALAPPDATA%\senapati\senapati.toml and copies .\skills
senapati doctor    # checks agents, local servers, MCP servers, skills, alert channels
senapati           # opens the terminal UI
```

To keep data somewhere else, use `--home <folder>` or set `SENAPATI_HOME`.

For local models, start the LM Studio server (`lms server start`) or Ollama, then check that
`base_url` in the config matches the port it reports.

## Terminal UI

| Key | Action |
| --- | --- |
| Tab / Shift+Tab | Switch between the Sessions, Plans and Agents views |
| ↑ ↓ | Select a session or plan |
| PgUp / PgDn, Ctrl+End | Scroll the transcript, or jump back to the bottom |
| Enter | Send a message (Sessions view) or draft a plan from a goal (Plans view) |
| Ctrl+N | New session with the default agent |
| Ctrl+Y / Ctrl+E | Approve the selected plan / edit it in `$EDITOR` (Notepad by default) |
| Ctrl+X | Stop the running turn (Sessions view) or cancel the plan (Plans view) |
| Ctrl+Q | Quit |

Commands typed into the input box:

```
/new [agent] [title]        /agent <name>             /skill <name> <prompt>   /skills
/plan [--per-provider N] <goal>                       /approve  /edit  /cancel  /stop
/cwd <folder>               /caveman off|lite|full|ultra|auto                  /quit
```

## Headless use

```powershell
senapati ask claude "explain src\main.rs"           # one turn; prints the answer and session id
senapati reply <session> "now add tests"            # continue that session
senapati plan --cwd C:\work\app "add a dark mode"   # draft only; prints the steps
senapati approve <plan>                             # run it and wait
senapati plan --yes "..."                           # draft and run in one go
senapati serve                                      # alerts and remote commands, no UI
senapati skills                                     # list discovered skills
```

## Alerts

Choose events in `[notify] events`: `plan_ready`, `step_done`, `step_failed`, `plan_done`,
`turn_done` and `limit_hit`. Secrets go to Windows Credential Manager, never into the config
file:

```powershell
senapati secret set telegram_bot_token
senapati secret set discord_webhook_url
senapati secret list
senapati notify-test
```

**Telegram:** create a bot with @BotFather and store its token. Set
`notify.telegram.enabled = true` and send the bot a message. Senapati shows
"ignored message from chat 123…"; add that number to `chat_ids`. Only one running Senapati
can poll a given bot.

**Discord:** create a channel webhook and store its URL. Discord alerts are one-way.

**WhatsApp:** link a second WhatsApp account as the bot. Alerts come from that number to the
numbers in `notify.whatsapp.to`.

> **Warning:** WhatsApp linking uses Baileys, an unofficial client, and breaks WhatsApp's
> Terms of Service. WhatsApp can ban the linked number. Use a spare number, never your main
> one. The link keys in `<home>\whatsapp-auth` give full access to that account, so keep the
> folder private.

```powershell
senapati whatsapp link      # scan the QR code with the BOT phone: Linked devices > Link a device
```

### Remote commands

Allowed Telegram chats and WhatsApp numbers can send these commands:

```
/status  /plans                                     always allowed (read-only)
/plan <goal>  /approve <plan>  /cancel <plan>        need notify.remote_control = true
/ask [agent] <prompt>  /reply <session> <text>  /stop <session>
```

Remote control is off by default. When it is on, anyone who controls an allowed chat can make
agents edit files on this machine.

## Configuration notes

- `[[agents]]`: `kind` is `claude-cli`, `codex-cli`, `openai-compat` or `cli`. Each agent can
  set `model`, `mcp`, `fallback`, `caveman` and `notes`. The planner reads `notes`, so use them
  to describe an agent's strengths and limits on this machine.
- `[mcp.<name>]`: `command`, `args`, `env`, and an optional `tools` allowlist. The allowlist
  only applies to local models; REA alone has over 200 tools, which floods a small model.
- `permission_mode` (claude) and `sandbox` (codex) decide what agents may do without asking.
  Headless agents cannot ask you, so anything not allowed is refused.

## Known limits

- **Codex on native Windows is read-only.** Codex currently forces `workspace-write` back to
  `read-only` ([openai/codex#34958](https://github.com/openai/codex/issues/34958)). The
  default config tells the planner so, and codex gets review and analysis steps. Remove that
  `notes` line once codex can write files here.
- Agents cannot ask for permission mid-turn. Anything outside `permission_mode` or `sandbox`
  is refused, and the agent reports it.
- A step counts as failed only when its agent says so (`STEP FAILED:`) or errors out.
  Senapati does not check the work itself.
- Local models answer in one block (no streaming), with at most 8 tool rounds per turn.
- Answers to remote `/ask` and `/reply` go to every enabled channel, not only the one that
  asked.

## Layout

```
src/agents/      claude, codex, openai-compatible and generic CLI adapters
src/engine.rs    sessions, turns, provider limits, failover, alerts
src/plan.rs      planner prompt, plan validation, parallel executor
src/layers.rs    caveman layer, skill injection, step handoff contract
src/mcp.rs       MCP client bridge for local models
src/notify/      Telegram, Discord, WhatsApp
src/remote.rs    chat commands
src/tui.rs       terminal UI
sidecars/whatsapp/  Baileys linked-device client (Node)
skills/          vendored skills (see skills/README.md)
```

## License

MIT, see [LICENSE](LICENSE). The vendored skills in `skills/` keep their own MIT license
([skills/LICENSE.replica-skill](skills/LICENSE.replica-skill)).
