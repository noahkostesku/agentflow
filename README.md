# agentflow

**One live view of every coding agent you run.**

agentflow is a full-screen terminal visualizer for coding-agent sessions:
Claude Code, Codex CLI, and more to come. Little agents boot up, show faces,
pass glowing data packets to each other, and celebrate when they finish. Run
Claude Code and Codex in the same repo and `--source all` shows both side by
side, on one clock, with one cost total.

It only reads the session files each harness already writes. Nothing to
install into your agents, no hooks, no proxy.

![agentflow demo](agent-flow-vid.gif)

## Supported harnesses

| harness | status | sessions read from |
|---------|--------|--------------------|
| Claude Code | verified on 2.1.328, subagents and agent teams included | `~/.claude/projects/` |
| Codex CLI | verified on 0.170.0, subagents included | `$CODEX_HOME/sessions/` (default `~/.codex/sessions/`) |
| pi | planned | `~/.pi/agent/sessions/` |
| opencode | planned | `~/.local/share/opencode/opencode.db` |
| Gemini CLI | planned | |

"Verified" means checked against real sessions from that version, not only
synthetic fixtures. Formats change between releases; if a newer version
renders wrong, please open an issue with the version. To add a harness, see
[docs/adapters.md](docs/adapters.md).

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
agentflow                    # live: the newest session for this directory, any harness
agentflow --source codex     # only Codex sessions
agentflow --source all       # every harness's session for this directory, together
agentflow --demo             # looping scripted demo (no session needed)
agentflow --demo --source all  # the demo with Claude Code and Codex side by side
agentflow wrapped            # your month with agents, as eight animated slides
agentflow events --json      # the session as normalized JSON events, live
```

| flag | meaning |
|------|---------|
| `--source <name>` | which harness to watch: `claude`, `codex`, or `all` for the mixed view (default: the most recently active session in any of them). `pi`, `opencode` and `gemini` are accepted but not built yet. |
| `--demo` | replay the scripted demo session on a loop |
| `--calm` | disable glitch, shimmer, and the finish wave (steady, low-motion) |
| `--session <id>` | watch a specific session id (pins it: no auto-switch) |
| `--project <path>` | watch the session for another directory (relative paths, `~`, symlinks and trailing slashes are resolved the way the harnesses record them) |
| `--no-cost` | hide the cost column, header total, and `+$` flashes |
| `--no-achievements` | no achievement popups |
| `--no-recap` | no recap card at the end |

**Controls:** `q` quit · `v` cycle views (tree → orbit → timeline) · `↑`/`↓` select
an agent · `Enter` agent details · `Esc` back (out of a panel, then clears the
selection, then back to the tree, then quits) · `s` stats · `t` timeline · `l` lifetime ·
`?` key hints · `c` toggle calm · `space` pause (demo) · `r` restart (demo) / refresh
now (live). The key hints show for ~3s on startup and on `?`.

## Views

`v` cycles three views of the same session; the header shows which one is up
(`v  tree · orbit · timeline`, the current one in mauve). Panels (`s`, `l`, the
detail view) open over the current view, and `Esc` returns to it.

- **tree**: the default. The orchestrator above a branch per agent, plus a log.
- **orbit**: the orchestrator in the center and its agents evenly spaced on a
  dim elliptical ring, starting at the top (up to 8; past that the busiest and the
  selected one get the seats). Each agent shows a face, its name, a status line,
  and a 6-cell braille trace that fills as it works. Dotted spokes carry dispatch
  and result packets (`· • ●`) between the center and each agent, so you can see
  who is waiting on whom. When one agent's result is fed into another agent's
  task, a temporary dotted chord joins the two and the packet travels along it
  instead (see *handoffs* below). The bottom line shows the latest dispatch,
  handoff, or result, e.g. `reviewer → fixer  findings 1.4kb`. It needs about
  25 rows; in a smaller terminal it falls back to the tree and says so.
  `↑`/`↓` and `Enter` work here too.
- **timeline**: swimlanes across the whole session (also on `t`; see below).

### Mixed view: `--source all`

Every harness's session for the project, together, for when you have Claude
Code and Codex working in the same repo at once:

- **tree**: grouped by harness. Each group has a dim label (`claude`,
  `codex`, with its share of the cost), its own root (`orchestrator`, or
  `you` when that harness's agent works alone), and its agents. The log below
  tags each line with its harness.
- **orbit**: one ring per harness, side by side, with the harness name dim
  above each. If they don't fit side by side they stack, given the rows; in a
  terminal too small for either, it shows the tree and says so.
- **timeline**: lanes grouped by harness, a dim divider and label between
  groups, all on one time axis.
- **header**: `agentflow // all // <project>` and the combined cost.
- **`s` stats**: a line per harness (agents, tool calls, cost, tokens) above
  the per-agent lines.

It shows the sessions active right now (written to in the last 10 minutes),
one per harness. If none is active it shows each harness's most recent
session for the project. Every ~2s it looks again: a harness that starts
working joins the view, and a newer session in a harness replaces the old one.

All sessions share one clock: `00:00:00` is the first agent's start in any
harness. `↑`/`↓` select across all agents, and the detail view names the
agent's harness.

**Handoffs** are detected, not guessed: a finished agent's result (or its agent
id) has to show up in a later agent's prompt, either whole or in at least two
distinctive lines (or one long one). A direct message from one agent to another
draws a chord too: a teammate's `SendMessage` in Claude Code's agent teams, a
subagent's `send_message` in Codex.

**Validators** (optional) are roles you name in the config, e.g.
`validators = ["test-runner", "reviewer"]`. In the orbit, a validator idles as
`validator`, shows `validating ⠹` while it works, then `✓ pass` or `✗ fail` (soft
red): an explicit `Verdict: …` line in its result wins, then the exit status of
its last test command, then its result's wording (`LGTM`, `changes requested`, …).
This is display only. agentflow just reads transcripts, so naming a validator
changes how it's drawn and gates nothing in your harness. None are set by default;
`--demo` shows the two above so you can see what they look like.

## Agents up close

**Rarity** comes from the model and sits just before each agent's name:
`·` haiku (common) · `◆` sonnet (rare) · `✦` opus or fable (legendary).

**Personality** comes from what an agent actually does with its tools: **reader**
(read / grep / glob), **builder** (edit / write), **runner** (shell commands),
**researcher** (web fetch / search). The biggest share wins; edits and web calls
count double, since every agent reads but only some build or research. Shown in
the detail view and the stats panel.

**Detail view** — `↑`/`↓` moves a mauve `›` marker through the agents (one that
isn't on screen is swapped into the tree), `Enter` opens it: rarity, role, model,
personality, state, elapsed, cost, its task, the last nine tool calls
(`time · tool · short arg`, e.g. `bash  cargo test -q`, `read  render.rs`), and the
first lines of its final result. `Esc` goes back.

**Stuck alert** — a running agent whose transcript has gone quiet for 2+ minutes
dozes off: `(-_-)` with a slow `z z z` drifting up into the row above, and
`idle 2m` in amber. Any new transcript entry wakes it. Set the threshold with
`stuck_after` (seconds; `0` turns it off). An agent with a tool call still in
flight (a slow build or test run: a `tool_use` with no `tool_result` yet) isn't
stuck: it stays awake and reads `running 2m` with a slow spinner. It only dozes
when nothing is in flight.

## Cost, stats & achievements

Each row shows the agent's **model** (`haiku` / `sonnet` / `opus` / `fable`, or
for OpenAI models their name, `luna` / `sol` / …, dim, after its name) and its
**cost so far**, which briefly flashes `+$0.02` as each call lands. The header
shows the **session total** (orchestrator + every agent), and in the mixed view
the total across harnesses.

**Costs are API-equivalent, not your bill.** They're computed from each
response's token usage (input, output, cache reads and writes) at the
provider's pay-per-token list prices (Anthropic's for Claude models, OpenAI's
for GPT models), which is why they're shown with `≈`. If you use a
subscription (Claude Pro/Max, ChatGPT Plus/Pro) you aren't billed per token,
and batch or priority discounts and fast mode aren't modeled either. Treat the
number as "what this usage would cost on the API". Rates live in one table,
[`src/pricing.rs`](src/pricing.rs), each with the date it was last checked:
exact model ids first, then a per-family fallback so a new point release
still gets a sane price. A model with no known price counts as $0 and puts a
`?` after the header total; the stats panel names it. You can override rates
in the config without rebuilding.

**`s` stats** — per agent: a tokens/sec sparkline over its lifetime, average
tok/s, cache-hit %, and cost; the session's cache-hit %; the most-used tools; how
parallel the agents ran (`team` line, see below); and the four most-touched files
as braille heat bars.

**`t` timeline** — swimlanes: one lane per agent, orchestrator first, across the
whole session. Each cell is braille as dense as the activity (tool calls and model
calls) in that slice of time: mauve while active, dim while waiting or idle, a red
`┃` where an error landed. The **critical path** (the chain of agents that set how
long the session took) is drawn in brighter mauve. Below: a time axis and the
parallelism line.

**Parallelism** (timeline and stats), measured over each agent's run from dispatch
to finish, across the window from the first dispatch to the last agent's end:

- **peak**: most agents running at once
- **avg 1.7×**: average agents running over that window (total agent time ÷ wall-clock)
- **2+ active 42%**: share of the window with two or more agents running
- **time saved**: total agent time vs wall-clock, e.g. `8m of work in 3m · saved 5m`
  (in the timeline's header; the stats line drops it when the terminal is too narrow)

The critical path starts at the agent that ended last and steps back to the agent
it waited on: the last one to finish before it was dispatched (a handoff to it
breaks ties). Agents that ran alongside it, off that chain, didn't change how long
the session took.

**`l` lifetime** — totals across every session you've watched: sessions, agents,
spend, tokens, cache savings, fastest agent, achievements collected, and the same
per harness (sessions, agents, cost, tokens). Needs `persistence` (on by
default); stored in `~/.agentflow/lifetime.json`, one entry per session id with
the harness it ran in, so re-watching a session never double counts. Entries
written before agentflow read more than Claude Code have no harness and count as
Claude Code.

**Achievements** pop up one line at a time, fade after ~3s, and each fires once
per session. Only ones earned after agentflow attaches are announced.

| achievement | earned when |
|-------------|-------------|
| speedrun ✦ | an agent finishes in under 10s |
| pacifist ✦ | an agent finishes without editing a file |
| bug hunter ✦ | a test command (cargo test, pytest, npm test, go test, jest, …) fails, then a later one passes, in any agent |
| big spender ✦ | one agent costs more than $0.50 |
| team player ✦ | 3+ agents run at once |

Personality and rarity also show in the stats panel.

**Recap card** — when every agent has finished, the log is replaced by a recap:
total cost and tokens, MVP (the agent with the most output tokens), longest task,
and cache savings (what the cache reads would have cost as plain input). Any key
dismisses it; a new batch of agents gets a new recap.

## Config

On first run agentflow creates `~/.agentflow/config.toml` with every setting
commented out at its default. CLI flags win over the file; an invalid file falls
back to defaults with a warning.

```toml
achievements = true   # achievement popups
recap = true          # recap card when all agents finish
cost = true           # cost column + header total
flashes = true        # "+$0.02" flash per call
persistence = true    # lifetime totals for the `l` view and `wrapped`
accent = "#c4a5b4"    # the one accent color (dusty mauve)
stuck_after = 120     # seconds of silence before an agent dozes (0 = off)
validators = ["test-runner", "reviewer"]  # optional: pass/fail labels in the orbit (default: none)

[pricing.opus]        # USD per million tokens; key by family or exact model id
input = 4.0           # unset fields keep the built-in rate
output = 20.0
```

## Wrapped

```sh
agentflow wrapped                       # the last 30 days (default)
agentflow wrapped --week                # the last 7 days
agentflow wrapped --all                 # everything on record
agentflow wrapped --export wrapped.svg  # the outro card as an SVG, then exit
```

Eight full-screen slides, each animating in (numbers count up, bars fill, text
types out). `space` / `→` next, `←` back, `q` quit:

1. **your agents, this month** — agents and sessions
2. **what it cost** — API-equivalent usage and what the cache saved
3. **your tools** — per harness: sessions, agent time and API-equivalent usage,
   with a small braille bar of agent time
4. **your top agent** — a character card for the role with the most output
   tokens: face, rarity (its most-used model), the harness it ran in,
   personality, runs
5. **your busiest hours** — a braille bar chart of tool calls by hour of day
6. **time saved** — total agent time vs the wall-clock it took, as two bars
   ("11h20m of agent work in 6h05m"), and the difference: time saved by running
   agents in parallel
7. **orchestrator type** — from average agents per session: soloist (≤1),
   duo (~2), conductor (3–4), swarm lord (5+)
8. **outro** — a summary card

`--export` writes the outro as a 1200×675 SVG (X's card size): `#0a0a0a`
background, your accent color, monospace type. With no history in the chosen
range yet, wrapped shows demo data and says so.

It reads `~/.agentflow/lifetime.json`, which agentflow fills while you watch
sessions (with `persistence` on). Sessions recorded before this version have no
start time or per-agent detail, so they only count toward `--all` totals; sessions
recorded before time saved existed are left out of it.

## How it works

agentflow is **read-only** toward every harness. It watches the session files
they already write (Claude Code's transcripts under `~/.claude/projects/`,
Codex's rollouts under `~/.codex/sessions/`) and renders them at ~20fps. It
installs no hooks, writes nothing to any harness's directories, and never
touches your sessions. Its only writes are its own `~/.agentflow/config.toml`
(created once) and, with `persistence` on, `~/.agentflow/lifetime.json`.

Each harness has an **adapter** that translates its files into one normalized,
versioned [event schema](docs/schema.md). Every view, stat and feature reads
only those events, so they work the same for every harness. See
[docs/architecture.md](docs/architecture.md).

With no flags it picks, in order:

1. the most-recently-active session for your **current directory**, in any
   supported harness, so running it in a repo watches that repo's session;
2. otherwise the **most recently active session across all projects**, showing
   that project's name in the header.

`--source <harness>` narrows that to one harness; `--source all` shows one
session per harness together (see *Mixed view*). "Active" counts writes from
background subagents too, so a session whose agents are still working isn't
mistaken for idle. Every ~2s it re-scans, and if a newer session starts in the
same directory it switches to it automatically (unless you pinned one with
`--session`).

Subagents appear the instant they're dispatched, and each one's own file is
tailed for live tool activity, completion, and result size. Nothing is
hardcoded to any one project.

## Events: `agentflow events --json`

The same normalized stream the views use, for your own tools: one JSON object
per line on stdout, the session so far and then live as it grows.

```sh
agentflow events --json                       # follow the newest session here
agentflow events --json --once                # what's on disk now, then exit
agentflow events --json --source all          # every harness's session, interleaved
agentflow events --json | jq 'select(.type == "tool_started") | .arg'
```

```json
{"v":1,"harness":"codex","session":"01a1f49c","ts":1795105280113,"type":"tool_started","agent":"main","tool":"bash","label":"cargo","kind":"run","arg":"cargo test","blocking":true,"test":true}
```

Every event carries the schema version, its harness and session id, so a
mixed stream can be split back apart. The fields and rules are documented in
[docs/schema.md](docs/schema.md). It exits quietly when the reader goes away
(`| head`).

## Roles

A session's main agent is the one working row, named after its harness
(`claude`, `codex`) under **you** (`◆`), until it spawns a subagent. From then on
it's the **orchestrator** (`◆`) and each subagent gets a row.

Codex subagents are named by their nickname (`socrates`), with their path as a
dim tag where there's room (`socrates · repo_overview`). Claude Code subagents
are named by role:

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

> Note on live waiting/error states: neither harness records a permission prompt
> while it's open, so agentflow can't see one as it happens. It shows **waiting**
> when a tool call (shell, edit, web) hasn't returned for ~10s — that could be a
> permission prompt or just a slow command — and **error** when a tool was denied
> or blocked. Both are best-effort from read-only session files, no hooks.

## Why

Multi-agent runs are a wall of scrolling text, often in several terminals at
once; it's hard to feel what your agents are actually doing. agentflow turns that firehose into a glanceable, calm little
dashboard — who's working, who's blocked, who's done — and makes watching your
agents work genuinely fun.

## License

MIT — see [LICENSE](LICENSE).
