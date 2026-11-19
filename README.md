# agentflow

A cute cyberpunk terminal visualizer for Claude Code multi-agent sessions — little agents boot up, show faces, pass glowing data packets, and celebrate when they finish.

![agentflow demo](demo.gif)

## Install

```sh
cargo install --git https://github.com/noahkostesku/agentflow
```

Or from source (needs Rust and a truecolor terminal):

```sh
git clone https://github.com/noahkostesku/agentflow
cd agentflow
cargo build --release      # -> target/release/agentflow
```

Optional alias so it runs anywhere:

```sh
echo 'alias aflow="agentflow"' >> ~/.zshrc && source ~/.zshrc
```

## Usage

```sh
agentflow            # live: watch the session for the current directory
agentflow --demo     # looping scripted demo (no session needed)
```

| flag | meaning |
|------|---------|
| `--demo` | replay the scripted demo session on a loop |
| `--calm` | disable glitch, shimmer, and the finish wave (steady, low-motion) |
| `--session <id>` | watch a specific session id (pins it: no auto-switch) |
| `--project <path>` | watch the session for another directory |

**Controls:** `q` / `Esc` quit · `c` toggle calm · `space` pause (demo) · `r` restart (demo) / refresh now (live)

## How it works

agentflow is **read-only**. It watches the transcript `.jsonl` files Claude Code
already writes under `~/.claude/projects/` and renders them at ~20fps. It installs
no hooks, writes nothing to `~/.claude`, and never touches your sessions.

With no flags it picks, in order:

1. the most-recently-active session for your **current directory**, so running it
   in a repo watches that repo's session;
2. otherwise the **most recently active session across all projects**, showing
   that project's name in the header.

"Active" counts writes from background subagents too, so a session whose
agents are still working isn't mistaken for idle. Every ~2s it re-scans, and if
a newer session starts in the same directory it switches to it automatically
(unless you pinned one with `--session`).

It reads the main transcript for subagent spawns (so an agent appears the instant
it's dispatched) and tails each subagent's own transcript for live tool activity,
completion, and result size. Nothing is hardcoded to any one project.

## Roles

The main session is the **orchestrator** (`◆`). Each subagent is named by role:

| spawn | role |
|-------|------|
| `Explore` | scout |
| `general-purpose` | the role from a `role: task` description (e.g. `test-runner: run cargo tests` → test-runner); otherwise operator, or researcher if the task looks web/research-y |
| `Plan` | architect |
| custom `subagent_type` | used as-is (reviewer, test-runner, fixer, …) |

Per-agent status: booting `(·_·)→(•_•)` · active `(•‿•)` (reading glances
`(•_• )`, commands `(•o•)`) · waiting `(·_·)` in amber · done `(^‿^)` · error
`(×_×)` in red. Progress is indeterminate while an agent runs and snaps full when
it returns.

> Note on live waiting/error states: Claude Code records permission *decisions*
> only when they resolve, so agentflow can't see a prompt as it happens. It shows
> **waiting** when a tool call (shell, edit, web) hasn't returned for ~10s — that
> could be a permission prompt or just a slow command — and **error** when a tool
> was denied or blocked. Both are best-effort from a read-only transcript, no hooks.

## Why

Multi-agent runs are a wall of scrolling text; it's hard to feel what your agents
are actually doing. agentflow turns that firehose into a glanceable, calm little
dashboard — who's working, who's blocked, who's done — and makes watching your
agents work genuinely fun.

## License

MIT — see [LICENSE](LICENSE).
