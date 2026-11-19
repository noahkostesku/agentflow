# Architecture

agentflow only observes. It reads the session files coding-agent harnesses
already write. It never sits in the request path, and it never writes to any
harness's directories.

The core is a normalized, versioned [event schema](schema.md). Adapters
translate each harness's logs into it. Everything downstream consumes only
the schema.

```
  harness session files          adapters             normalized            consumers
  ─────────────────────          ────────             ──────────            ─────────
  ~/.claude/projects/…  ──►  claude adapter   ─┐                      ┌─►  tree / orbit / timeline
  ~/.codex/sessions/…   ──►  codex adapter    ─┤                      ├─►  stats · cost · parallelism
  (planned) pi          ──►  pi adapter       ─┼─►  event schema v1 ──┼─►  lifetime · achievements · wrapped
  (planned) opencode    ──►  opencode adapter ─┤    (session, agent,  └─►  `agentflow events --json`
  (planned) gemini      ──►  gemini adapter   ─┤     tool call,             (stdout, for other tools)
  (later) OTLP export   ──►  otel receiver    ─┘     handoff, usage,
                                                     done/error)
```

**Rule:** no view, stat, or feature may read harness-specific data directly.
Only the schema.

## In the code

| layer | where | reads | produces |
|-------|-------|-------|----------|
| adapters | `src/adapters/<harness>.rs` | that harness's files | `schema::Event`s, via the `SourceAdapter` trait |
| schema | `src/schema.rs` | | the event types (`docs/schema.md`) |
| discovery | `src/discovery.rs` | each adapter's `find` / `activity` | which session(s) to watch |
| reducer | `src/live.rs` | schema events only | the render model (`model::Session`) for one session |
| mixed view | `src/mixed.rs` | `model::Session`s | one `Session` with a group per harness |
| inbox | `src/inbox.rs` | each session's `live::Pulse` | one line per active session, by urgency |
| notifications | `src/notify.rs` | inbox entries | desktop notifications on state changes |
| processes | `src/procs.rs` | `ps`, `lsof` / `/proc` | which `claude` / `codex` processes run where (closed sessions) |
| durations | `src/duration.rs` | | the one duration format (`42s`, `17m 55s`, `1h 02m`) |
| consumers | `render.rs`, `orbit.rs`, `stats.rs`, `lifetime.rs`, `wrapped.rs` | `model::Session` only | frames, stats, lifetime history |

`SourceAdapter::poll()` returns whatever is new on disk as events. The live
loop calls it about three times a second and folds the events into the
`Session` that every view draws. The demo builds a `Session` directly, from a
script.

Pricing is downstream too: `usage` events carry token counts and a model id,
and `pricing.rs` turns them into dollars at the provider's list prices
(API-equivalent cost, not a bill).

## Discovery

Each adapter knows where its harness stores sessions
(`adapters::<harness>::find`, `activity`). `discovery.rs` picks among them:

- **one session** (default, or `--source <harness>`): `--session` in
  whichever harness has it, else the project's most recently active session
  in any of the chosen harnesses, else the newest anywhere. Every ~2s it
  looks again and hops to a newer session in the same directory.
- **mixed** (`--source all`, `discovery::find_mixed`): each harness's most
  recently active session for the project, keeping only those active in the
  last 10 minutes; if none is, each harness's most recent. With none for the
  project, the same rule over every project.

- **inbox** (`agentflow inbox`, `discovery::find_active_at`): every
  session in every project and harness written to in the last
  `[inbox] active_minutes` (30), subagents' writes included. Each adapter
  lists its own (`adapters::<harness>::active`).

## The inbox and notifications

`inbox::Inbox` holds one `live::Live` per active session, the same reducer
the views use. Instead of a render model it asks each for a `Pulse`
(`Live::pulse`): is any agent waiting on the user (an explicit `waiting`
event, or a `blocking` call open 10s+), in error, running, with a call in
flight; its latest tool call, last message, agent counts and cost.
`inbox::classify` turns a pulse into a state (waiting, error, stuck, done,
working, idle) and the time it began; rows sort by that. A session counts
by its own latest event, not its file's mtime (harnesses append bookkeeping
records long after the work ends).

`notify::Notifier` watches those states and emits one notification per
change per session; `notify::Watcher` keeps an `Inbox` current on its own
clock for both `agentflow notify` (headless) and `agentflow inbox --notify`.
Reviewed marks live in `~/.agentflow/reviewed.json`. `inbox::settle` checks
open sessions against `procs::scan` (every 10s) and marks the ones whose
process is gone as closed; with no process view it falls back to 12h of quiet.

## The mixed view

`mixed::Mixed` holds one `live::Live` per harness. To draw a frame it builds
each one with `Live::build_at(t0)`, where `t0` is the earliest start across
them, so every harness's session is on one clock. `mixed::merge` then
concatenates them into one `Session`:

- `agents`, `orch_calls`, `events` and `handoffs` hold every harness's,
  in group order (handoff indexes shifted), so totals, stats, achievements
  and selection work across harnesses unchanged;
- `groups` keeps each harness's own `Session` and where its agents start in
  `agents`, so the views can draw a group per harness, each with its own
  root (`orchestrator` or `you`), critical path and orbit ring.

A single-session `Session` has no groups; views check `Session::mixed()`.

Lifetime history is recorded per harness session, not per mixed view:
`Mixed::parts` builds each session on its own clock, and each lifetime
entry records its `harness`.
