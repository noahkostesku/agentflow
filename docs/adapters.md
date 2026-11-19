# Writing an adapter

An adapter teaches agentflow to read one coding-agent harness. It reads the
session files that harness already writes and turns them into
[schema v1 events](schema.md). Everything else (the tree, orbit and timeline
views, stats, cost, lifetime history, wrapped, `agentflow events --json`)
works from those events, so a new harness gets all of it for free.

Adapters only observe. They never write to a harness's directories, never sit
in its request path, and never need hooks or a plugin installed in it.

A new adapter is three things:

1. **one file**, `src/adapters/<harness>.rs`
2. **a synthetic fixture**, `tests/fixtures/<harness>/…`
3. **tests** in that file's `mod tests`

plus a few one-line registrations (listed below). The Codex adapter
(`src/adapters/codex.rs`) is the best one to copy: one JSONL file per
session, subagents in sibling files.

## 1. The adapter file

Start with a module comment that records the format **as observed**, with the
harness version you observed it on: where sessions live, which records matter,
and how they map to events. When the format changes, that comment is what
the next person diffs against.

The file provides:

```rust
pub struct PiAdapter { /* path, read offsets, per-agent state */ }

impl SourceAdapter for PiAdapter {
    fn harness(&self) -> Harness;      // Harness::Pi
    fn session_id(&self) -> &str;      // the harness's own id
    fn path(&self) -> &Path;           // the session's primary file
    fn poll(&mut self) -> Vec<Event>;  // what's new since the last call
}

impl PiAdapter {
    pub fn open(path: &Path) -> Self;
}

/// Where the harness keeps sessions (honor its env override, if any).
pub fn sessions_root() -> PathBuf;

/// A session to watch: by id, else the newest for `project`, else (with
/// `fallback`) the newest anywhere.
pub fn find(root: &Path, id: Option<&str>, project: Option<&Path>, fallback: bool) -> Option<PathBuf>;

/// When the session last showed signs of life (subagents' files included).
pub fn activity(root: &Path, path: &Path) -> SystemTime;
```

`poll()` is called about three times a second. The first call starts with
`session_started` and replays the session so far; later calls return only
what was appended. Keep a byte offset per file and use
`adapters::tail(path, &mut offset)`, which returns only complete lines,
survives truncation and never stalls on invalid UTF-8.

Other helpers in `src/adapters/mod.rs`: `iso_to_ms` (RFC 3339 UTC → epoch ms),
`now_ms`, and `is_test_command` (does a shell command run a test suite; set
`test` on its `tool_started`).

### Mapping to events

Follow the ordering rules in [schema.md](schema.md#ordering-rules). The ones
adapters most often get wrong:

- **The main thread is the agent `main`.** Until a subagent spawns it's the
  one working row; after that it's the orchestrator. Emit `agent_finished`
  for `main` at the end of each turn. Its next `tool_started` reopens it.
- **`agent_spawned` comes before anything else about that agent**, and may
  be re-emitted as you learn more (consumers merge).
- **`usage` ids:** if a response is logged more than once as it streams,
  reuse its id. Repeats update the call instead of billing it again.
- **Cached input:** report `input` *without* cached tokens and put those in
  `cache_read`. Some harnesses count cached tokens inside input (Codex does);
  subtract them.
- **Copied history:** if a subagent's file starts with a copy of its
  parent's conversation (Codex's does), skip it, or its cost counts twice.
- **Heartbeats:** a record that maps to nothing else should still produce a
  `heartbeat`, so a busy agent never looks stuck.
- **`blocking`:** set it on tool calls that can wait on an approval prompt.
  If the harness records approvals itself, emit `waiting` instead.

Map tool names to the shared lowercase set where one exists (`bash`,
`read`, `edit`, `write`, `grep`, `glob`, `webfetch`, …). Personality and
achievements key off those names. Set `file` and `edit` for anything that
reads or changes a file, so the stats panel's file heatmap works.

## 2. The synthetic fixture

Never commit a real session: they hold prompts, code, paths and sometimes
secrets. Write a small file by hand that mirrors the real one's **record
types, field names and ordering**, with made-up content. Cover at least:

- the session header (id, cwd, model)
- a read, a shell command, and an edit
- a test command that **fails** (non-zero exit) and one that passes
- token usage on two or more responses, cached input included
- the end of a turn, with a final message
- subagents (a spawn, the child's file, its result), if the harness has them

Name the files the way the harness does, so `find` can be tested on them too.

## 3. Tests

In the adapter's `mod tests`:

- **events from the fixture:** `include_str!` the fixture, write it to a
  temp dir, `open` it, `poll`, and assert the event sequence: the spawns,
  each tool call with its `kind`, `file` and `test` flags, the failing
  test's `error: true`, usage totals, and the finish.
- **incremental reads:** append half the fixture, poll, append the rest,
  poll again. Nothing is duplicated or lost.
- **it renders:** feed the adapter to `live::Live`, `build` a `Session`,
  and check the rows, names and cost come out right.
- **discovery:** `find` by id and by project in a temp `sessions_root`, and
  `activity` counting subagent files.

Then check it against a **real** session, which is what "verified" means in the
README's table:

```sh
agentflow events --json --once --source <harness> --project ~/some/project
agentflow --source <harness> --project ~/some/project
agentflow --source all --project ~/some/project      # next to the others
```

## 4. Registering it

A handful of lines outside the adapter file:

| where | what |
|-------|------|
| `src/schema.rs` | a `Harness` variant and its `name()` |
| `src/adapters/mod.rs` | `pub mod <harness>;` |
| `src/discovery.rs` | add it to `SUPPORTED`, and an arm in `Found::open`, `activity`, `find_in`, plus its root in `Roots` |
| `src/main.rs` | move it from "planned" to a real `Source` in `Source::harnesses`, and its root in `no_session` |
| `src/pricing.rs` | rates for its models, with the date you checked them (unpriced models count as $0 and show a `?`) |
| `README.md` | its row in the supported-harnesses table, with the version you verified |
| `docs/schema.md` | its value in the `harness` field |

The views, stats, lifetime records, wrapped and the mixed view
(`--source all`) need no changes.

## Notes on planned harnesses

Observed on this machine on 2026-11-19. Neither pi's nor opencode's session
got past its first model call (the API key had no credits), so **no tool
calls have been seen in either format yet**. These notes say where to look,
not how tool calls are shaped. Build each adapter from a real session that
reads a file, runs a test and edits something.

### pi (1.5.2)

- **Where:** `~/.pi/agent/sessions/<project>/<time>_<uuid>.jsonl`, one JSONL
  file per session, e.g.
  `--Users-me-agentflow-playground--/2026-11-19T16-07-46-243Z_<uuid>.jsonl`.
  The folder name is the project path with `/` turned into `-`, wrapped in
  `--`, which makes `find` by project a directory lookup, like Claude Code's.
- **Records seen:** a session header with the `cwd` and a format version
  (`3`); a model change (to `gpt-6.1-sol`); the system prompt; the user's
  prompt; one assistant reply.
- **Usage:** each assistant reply carries token counts and **pi's own cost
  figure**. Still price from tokens, so pi's cost compares with the other
  harnesses'.
- **Turn end / errors:** the reply has a `stopReason`. The failed one had
  `stopReason: "error"` with the provider's message ("You have no credits
  remaining") and all-zero token counts. That should map to `agent_error`
  on `main`.
- **Unknown:** tool-call blocks and their results, and whether pi has
  subagents.

### opencode (2.3.2)

- **Where:** a SQLite database, `~/.local/share/opencode/opencode.db`, with
  tables `session_v2`, `session_message` and `session_part`. Not a log file,
  so this adapter will look different:
  - agentflow has no SQLite dependency yet. Add `rusqlite` with its
    `bundled` feature, and open the database **read-only**
    (`?mode=ro`) so agentflow can never lock or write it while opencode
    runs.
  - `poll` can't tail bytes. Track the last row seen per table (by rowid or
    timestamp) and query for newer ones.
  - `path()` is the database, which holds every session. `find` picks a
    session row by its directory, and `activity` should use that
    session's latest message time, not the file's mtime.
- **Records seen:** one session with three messages (the prompt, then an
  assistant reply on `gpt-6.1-terra` that failed with
  `credit_balance_exhausted`; the session ended `failed`), and **zero
  parts**. In opencode, text and tool calls live in `session_part`, so that
  table is where the tool calls will be.
- **Usage:** messages record per-call token usage.
- **Discovery:** the session records the directory it ran in. The one seen
  was started from the home folder, so `--project` matching needs that
  directory, not the database's location.

### Gemini CLI

- v0.1.7 (June 2025) writes only `logs.json` per project: the user's prompts
  (session id, timestamp, text). No tool calls, model, tokens or replies, so
  an adapter for it could show little more than "a session exists". Newer
  versions save full chat files; build from one of those.
