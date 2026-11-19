# Changelog

All notable changes to agentflow. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Inbox** (`agentflow inbox`, or `i` from any view): every session active
  in the last 30 minutes, across all projects and harnesses, one line each,
  sorted by urgency: waiting on you, error, stuck, done but not reviewed,
  working, idle. Each line shows what it's doing or waiting on, agents
  running/total, time in that state and ≈cost. `Enter` opens the session in
  the usual views, `Esc` comes back, `r` marks a done session reviewed
  (kept in `~/.agentflow/reviewed.json`). `inbox --demo` shows a scripted one.
- **Notifications** (`agentflow notify`, headless, or `inbox --notify`): a
  desktop notification (`osascript` on macOS, `notify-send` on Linux) when a
  session starts waiting, errors, gets stuck or finishes. One per state
  change per session, silent on startup, with per-state toggles, quiet
  hours and a repeat guard in `[notify]`. `--dry-run` and `--once` for
  checking it.
- Config: `[inbox] active_minutes`, and `[notify]` (`waiting`, `error`,
  `stuck`, `done`, `quiet`, `repeat_after`).
- **Closed sessions** in the inbox: a waiting or working session whose
  `claude` / `codex` process is gone (terminal closed, process killed) shows
  dim as "closed while waiting" below the urgent states, instead of needing
  you forever. Processes are matched by session id on the command line, else
  by working directory; where they can't be seen, 12 hours of quiet marks a
  session stale.

### Changed

- **Waiting is detected more precisely.** Claude Code's `AskUserQuestion`
  and `ExitPlanMode` and Codex's `request_user_input` read as waiting the
  moment they're called, with the question shown. The "a call that can
  prompt hasn't returned for 10s" inference now follows each session's
  permissions: Claude Code's permission mode (a slow shell command in `auto`
  or `bypassPermissions` mode is running, not waiting; edits in
  `acceptEdits` don't wait) and Codex's approval policy (under `on-request`,
  only escalated commands can wait). This applies to every view, not just
  the inbox.
- An interrupted turn (Esc in Claude Code, `turn_aborted` in Codex) ends the
  turn instead of reading as a new prompt or a stuck agent.
- **One duration format everywhere** (tree rows, labels, recap, detail,
  timeline, stats, lifetime, wrapped, inbox): `42s`, `17m 55s` (seconds
  always two digits, so `2m 00s` keeps its width), `1h 02m` in aligned
  columns where a value ticks in place; labels and prose drop the padding
  and zero parts (`active ≤ 30m`, `8m of work in 3m`, `1h 2m`). A tree row's
  time always has a space before it (no more `⠴2673s`), and its progress bar
  is 7 cells to make room. A call in flight past `stuck_after` now reads
  `busy 2m 05s` (was `running 2m`), which fits the 12-column status.
- In a solo session (no subagents) the "you" row reads `waiting on claude`
  (or `codex`) while the agent works, and `all done ✓` after; `delegating`
  is kept for sessions with subagents.

## [0.2.0] - 2026-11-19

agentflow is now harness-agnostic: one live view of every coding agent you run.
0.1.0 watched Claude Code sessions in a single tree view; 0.2.0 adds Codex CLI,
a normalized event schema, two more views, cost and stats, lifetime history and
`wrapped`.

### Added

- **Event schema v1** (`docs/schema.md`): a normalized, versioned stream of
  session, agent, tool, usage, handoff and finish events. Every view and stat
  reads only the schema; each harness has an adapter that translates its own
  session files into it.
- **Codex CLI support** (verified on 0.170.0), subagents included: nicknames
  with their path as a tag (`socrates · repo_overview`), `send_message`
  handoffs, and copied parent history skipped so cost isn't counted twice.
- **`--source claude|codex|all`**. The default watches the most recently
  active session for the current directory in any supported harness.
- **Mixed view** (`--source all`): Claude Code and Codex sessions for the same
  project side by side on one clock, grouped by harness in the tree, one orbit
  ring per harness, timeline lanes grouped by harness, per-harness stats and a
  combined cost. `--demo --source all` replays a mixed session.
- **`agentflow events --json [--once]`**: the normalized event stream on
  stdout, live, for other tools.
- **Orbit view**: the orchestrator at the center, agents on a ring, dispatch
  and result packets along spokes, and chords for detected handoffs (a
  result quoted in a later agent's prompt, or a direct message).
- **Timeline view**: swimlanes per agent with activity density, wait and error
  marks, the critical path, and a time axis.
- **`v`** cycles tree → orbit → timeline; the header shows which is up.
- **Validators** (opt-in config): roles drawn as pass/fail gates in the orbit,
  from a verdict line, test exit status, or result wording.
- **Cost**: per-agent cost column, header total and `+$` flashes, estimated
  from token usage at Anthropic's and OpenAI's list prices and labeled as
  API-equivalent (`≈`). Rates are overridable in the config; unpriced models
  count as $0 and are flagged.
- **Stats panel** (`s`): per-agent tokens/sec sparklines, cache-hit rate,
  personality and cost; top tools; most-touched files as heat bars.
- **Parallelism stats**: peak agents, average concurrency, share of time with
  2+ agents running, and time saved (`49s of work in 36s · saved 13s`).
- **Detail view** (`↑`/`↓`, `Enter`): an agent's model, rarity, personality,
  state, task, last tool calls and result preview.
- **Rarity** by model family and **personality** (reader, builder, runner,
  researcher) by tool use.
- **Stuck alert**: an agent silent past `stuck_after` dozes (`(-_-)`, drifting
  z's, `idle 2m`); one with a tool call still in flight reads `running 2m`.
- **Achievements** (speedrun, pacifist, bug hunter, big spender, team player)
  and a **recap card** when every agent has finished.
- **Lifetime totals** (`l`), kept in `~/.agentflow/lifetime.json` per session,
  with a per-harness breakdown.
- **`agentflow wrapped`**: eight animated slides of your agents over the last
  week, month or all time (agents, API-equivalent usage, your tools per
  harness, top agent, busiest hours, time saved, orchestrator type, outro),
  plus `--export` of the outro card as a 1200×675 SVG.
- **Config file** `~/.agentflow/config.toml`, created on first run with every
  setting at its default; `--no-cost`, `--no-achievements` and `--no-recap`
  override it.
- A session's main agent shows as one working row named for its harness, under
  "you", until it spawns a subagent; then it's the orchestrator. Claude Code's
  main thread now reports its own tool calls.
- Docs: `docs/architecture.md`, `docs/schema.md`, and `docs/adapters.md` for
  writing a new harness adapter.
- Release binaries for macOS (arm64, x86_64) and Linux (x86_64, arm64), a
  shell installer and a Homebrew formula.

### Changed

- The header reads `agentflow // <harness> // <project>`, shortening to
  `af //` and then the project name with `…` in narrow terminals.
- The clock shows real elapsed time from `00:00:00`.
- `--project` paths are normalized the way the harnesses record them
  (relative paths, `~`, symlinks, trailing slashes).

### Fixed

- Auto-switching to a newer session no longer drops a session whose
  background agents are still working.
- Background agents, role names and error states are read correctly.
- The terminal is restored if agentflow panics.
- Tailing a transcript no longer stalls on invalid UTF-8.

## [0.1.0]

First release: a live tree view of a Claude Code session's subagents, with a
looping `--demo` and a `--calm` mode.

[0.2.0]: https://github.com/noahkostesku/agentflow/releases/tag/v0.2.0
