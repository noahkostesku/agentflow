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
  codex sessions        ──►  codex adapter    ─┤                      ├─►  stats · cost · parallelism
  pi sessions           ──►  pi adapter       ─┼─►  event schema v1 ──┼─►  achievements · wrapped
  opencode storage      ──►  opencode adapter ─┤    (session, agent,  └─►  `agentflow events --json`
  gemini sessions       ──►  gemini adapter   ─┤     tool call,             (stdout, for other tools)
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
| reducer | `src/live.rs` | schema events only | the render model (`model::Session`) |
| consumers | `render.rs`, `orbit.rs`, `stats.rs`, `lifetime.rs`, `wrapped.rs` | `model::Session` only | frames, stats, lifetime history |

`SourceAdapter::poll()` returns whatever is new on disk as events. The live
loop calls it about three times a second and folds the events into the
`Session` that every view draws. The demo builds a `Session` directly, from a
script.

Pricing is downstream too: `usage` events carry token counts and a model id,
and `pricing.rs` turns them into dollars.

Discovery, finding which session to watch, is still Claude-only
(`discovery.rs`). It becomes per-adapter when other harnesses land.
