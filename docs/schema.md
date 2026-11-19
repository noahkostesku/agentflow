# agentflow event schema v1

Every harness adapter turns its harness's session files into a stream of
**events** in this schema. Everything downstream (the tree, orbit and
timeline views, stats, cost, parallelism, achievements, wrapped, and
`agentflow events --json`) reads only these events, never a harness's own
files.

Source of truth: [`src/schema.rs`](../src/schema.rs). Every JSON example on
this page is parsed and re-serialized by a unit test
(`schema::tests::doc_examples_round_trip`), so this page and the code can't
drift apart.

## Envelope

Every event is one JSON object (one line in `agentflow events --json`) with
these fields, plus the fields of its `type`:

| field     | type    | meaning |
|-----------|---------|---------|
| `v`       | integer | schema version, `1` |
| `harness` | string  | `claude`, `codex` |
| `session` | string  | the harness's own session id |
| `ts`      | integer | epoch milliseconds when it happened, as the harness recorded it |
| `type`    | string  | one of the event types below |

Fields that are empty, `false`, or absent are omitted when serializing.
Readers must ignore fields they don't know: new optional fields can appear
within v1. Removing a field or changing its meaning bumps `v`.

## Agents

Most events name an `agent`: an id unique within the session. The session's
top-level thread is always the agent **`main`**. No agent spawns it, but an
adapter may emit `agent_spawned` for `main` (with no `parent`) to record what
the user asked as its `prompt`, again on each new prompt.

- Until any subagent spawns, `main` is **the working agent**: views show it
  as one row named after the harness (`claude`, `codex`) under "you". It
  finishes at the end of each turn (`agent_finished`) and its next
  `tool_started` means it's working again.
- Once a subagent spawns, `main` is **the orchestrator**: its usage is the
  orchestrator's cost, and every other agent is one of its subagents.

## Ordering rules

Adapters guarantee:

1. `session_started` comes first, once.
2. `agent_spawned` comes before any other event for that agent.
3. `agent_spawned` may be **re-emitted** as the harness reveals more about the
   agent (its transcript id, a more specific type). Consumers merge: a
   non-empty field replaces the old value, `ids` accumulate, and the spawn
   time is the first event's `ts`.
4. `tool_started` is emitted once per `call` id.
5. `usage` events that repeat an `id` update that model call (field-wise max of
   the token counts); they are not new calls.

## Event types

### `session_started`

A session began, or the adapter attached to it.

| field       | type    | meaning |
|-------------|---------|---------|
| `cwd`       | string? | project directory the harness ran in |
| `subagents` | bool    | the harness can run subagents under `main` (informational; the rule above decides how `main` is shown) |

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069298273,"type":"session_started","cwd":"/Users/me/code/parser","subagents":true}
```

### `agent_spawned`

A subagent was dispatched, or more became known about it (see rule 3).

| field        | type      | meaning |
|--------------|-----------|---------|
| `agent`      | string    | its id |
| `parent`     | string?   | who dispatched it (usually `main`) |
| `role`       | string    | display name: `scout`, `test-runner`, `socrates`, … |
| `tag`        | string?   | a short dim tag shown after the name where there's room (Codex: the agent's path, `repo_overview`) |
| `agent_type` | string?   | the harness's own type (`Explore`, `general-purpose`, …) |
| `task`       | string?   | short description of the assignment |
| `prompt`     | string?   | the full prompt (handoff detection reads it) |
| `alias`      | string?   | a name other agents address it by (agent teams) |
| `ids`        | string[]? | other ids it's known by (a prompt quoting one is a handoff) |

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069299000,"type":"agent_spawned","agent":"toolu_01A","parent":"main","role":"test-runner","agent_type":"general-purpose","task":"test-runner: run the parser tests","prompt":"Run cargo test and report failures.","ids":["a7f3"]}
```

A Codex subagent, named by its nickname with its path as the tag:

```json
{"v":1,"harness":"codex","session":"01a1f49c","ts":1795105275547,"type":"agent_spawned","agent":"01a1f49c-be7d","parent":"main","role":"socrates","tag":"repo_overview","task":"repo_overview","alias":"/root/repo_overview"}
```

What the user asked, recorded on `main` (no `parent`):

```json
{"v":1,"harness":"codex","session":"01a1f49c","ts":1795105271931,"type":"agent_spawned","agent":"main","role":"","prompt":"spawn 3 agents do a simple parallel task"}
```

### `tool_started`

An agent invoked a tool.

| field      | type    | meaning |
|------------|---------|---------|
| `agent`    | string  | |
| `call`     | string? | the call id; pairs with `tool_finished` |
| `tool`     | string  | normalized lowercase tool name (`bash`, `read`, `edit`, …; MCP tools keep only their own name) |
| `label`    | string  | short label for the face: the command's first word for shells (`cargo`), else the tool name |
| `kind`     | string  | `look` (reading, searching) or `run` (commands, edits, fetches) |
| `file`     | string? | the file it reads or writes |
| `edit`     | bool    | it modifies a file |
| `arg`      | string? | a few words of argument: `cargo test -q`, `lib.rs`, a host |
| `blocking` | bool    | it can block on an approval prompt; one pending for 10s+ reads as *waiting* |
| `test`     | bool    | it runs a test suite; its `tool_finished.error` is the outcome |

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069300500,"type":"tool_started","agent":"toolu_01A","call":"toolu_01B","tool":"bash","label":"cargo","kind":"run","arg":"cargo test -q","blocking":true,"test":true}
```

### `tool_finished`

A tool call returned.

| field    | type    | meaning |
|----------|---------|---------|
| `agent`  | string  | |
| `call`   | string? | the call id from `tool_started` |
| `error`  | bool    | it failed (non-zero exit, tool error) |
| `denied` | bool    | it was denied or blocked by permissions; puts the agent in error until its next tool call |

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069302000,"type":"tool_finished","agent":"toolu_01A","call":"toolu_01B","error":true}
```

### `usage`

Tokens billed for one model call. Costs are computed downstream from the
model id and the pricing table; a model with no price counts as $0.

| field            | type    | meaning |
|------------------|---------|---------|
| `agent`          | string  | |
| `id`             | string  | the model call's id (repeats update it, rule 5) |
| `model`          | string? | model id as the provider names it |
| `input`          | integer | uncached input tokens |
| `output`         | integer | output tokens |
| `cache_read`     | integer | input tokens read from cache |
| `cache_write_5m` | integer | tokens written to a 5-minute cache |
| `cache_write_1h` | integer | tokens written to a 1-hour cache |

A `usage` event with all-zero counts only names the agent's model.

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069300400,"type":"usage","agent":"toolu_01A","id":"msg_01X","model":"claude-haiku-5-5","input":12,"output":480,"cache_read":20311,"cache_write_5m":1200,"cache_write_1h":0}
```

### `output`

The agent produced text. Its latest output stands in for the result when an
agent finishes without handing one back.

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069300400,"type":"output","agent":"toolu_01A","text":"Running the parser tests."}
```

### `handoff`

The agent sent something directly to another agent (e.g. an agent-team
message). Handoffs where one agent's result is quoted in a later agent's
prompt aren't events: consumers detect those from `prompt` and `result`.

| field   | type    | meaning |
|---------|---------|---------|
| `agent` | string  | sender |
| `to`    | string  | recipient as the harness named it: an agent id, `alias`, `role` or `agent_type` |
| `what`  | string  | `message`, `findings`, `patch`, … |
| `bytes` | integer | size of what moved |

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069307000,"type":"handoff","agent":"toolu_01F","to":"rev","what":"message","bytes":21}
```

### `waiting`

The agent is blocked on the user (an approval prompt), as the harness
recorded it. Cleared by the agent's next tool or finish event. Harnesses that
don't record this rely on the `blocking` inference instead.

| field    | type    | meaning |
|----------|---------|---------|
| `agent`  | string  | |
| `call`   | string? | the call waiting for approval |
| `reason` | string? | e.g. `approval` |

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069303000,"type":"waiting","agent":"toolu_01A","call":"toolu_01C","reason":"approval"}
```

### `agent_finished`

The agent finished and handed back. For `main` in a harness without
subagents this ends a turn: its next `tool_started` means it's working again.

| field    | type     | meaning |
|----------|----------|---------|
| `agent`  | string   | |
| `result` | string?  | its final answer; replaces an earlier one |
| `bytes`  | integer? | size of what it handed back |
| `error`  | bool     | it finished in error |

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069305000,"type":"agent_finished","agent":"toolu_01A","result":"1 failing test: parse_header splits on every colon.","bytes":51}
```

### `agent_error`

The agent hit an error it may recover from (a rejected approval, an API
error). The agent shows as in error until its next tool call.

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069303500,"type":"agent_error","agent":"toolu_01A","message":"rejected"}
```

### `heartbeat`

The agent wrote something that produced no other event. It keeps the agent
from looking stuck. Every other event about an agent, except `agent_spawned`
and `agent_finished` (which its parent may report), also counts as a sign of
life.

```json
{"v":1,"harness":"claude","session":"8f2c1e","ts":1795069304000,"type":"heartbeat","agent":"toolu_01A"}
```
