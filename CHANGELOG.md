# Changelog

All notable changes to agentflow. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

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
