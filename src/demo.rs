//! The scripted demo session: parallel agents, then a handoff chain through
//! two validators. Replays on a loop.
//!
//! scout, researcher and a test-runner start in parallel (team player): the
//! test-runner (a validator) finds a failing parser test. Then the reviewer
//! (a validator) rejects the parser change; its findings are handed to the
//! fixer, whose patch is handed to a second test-runner, which passes it.
//!
//! `--demo --source all` replays [`mixed`] instead: a Claude Code session and
//! a Codex session working in the same project at once.

use crate::model::{Agent, Call, Event, Session, TestRun, Tool, ToolKind, Use, Usage};
use crate::pricing::{self, Pricing};
use crate::theme;

/// Validator roles the demo shows when the config names none, so the
/// pass/fail gates are on display.
pub const VALIDATORS: [&str; 2] = ["test-runner", "reviewer"];

/// Loop length: everything is done by 13.3s, the recap shows at 14.8s; the
/// rest leaves room to tour the views before it wraps.
const LENGTH: i64 = 20_000;

/// Build the demo session. `header` is the full header string, derived from
/// the real watched directory.
pub fn session(header: String, pricing: &Pricing) -> Session {
    let mut agents = vec![
        Agent {
            name: "scout".into(),
            spawn: 800,
            finish: Some(4000),
            tools: vec![Tool::look("read"), Tool::look("grep"), Tool::look("glob")],
            size: "2.1kb".into(),
            bob: 0,
            ..Agent::default()
        },
        Agent {
            name: "researcher".into(),
            spawn: 1200,
            finish: Some(5000),
            wait: Some((2900, 3700)),
            tools: vec![Tool::run("web_fetch"), Tool::look("read"), Tool::run("web_search")],
            size: "4.8kb".into(),
            bob: 3,
            ..Agent::default()
        },
        Agent {
            name: "test-runner".into(),
            spawn: 1600,
            finish: Some(4600),
            tools: vec![Tool::run("pytest"), Tool::look("read")],
            size: "0.4kb".into(),
            bob: 8,
            ..Agent::default()
        },
        Agent {
            name: "reviewer".into(),
            spawn: 5200,
            finish: Some(7400),
            tools: vec![Tool::look("read"), Tool::look("grep")],
            size: "1.4kb".into(),
            bob: 6,
            ..Agent::default()
        },
        Agent {
            name: "fixer".into(),
            spawn: 8000,
            finish: Some(10200),
            tools: vec![Tool::run("pytest"), Tool::run("edit")],
            size: "0.9kb".into(),
            bob: 2,
            ..Agent::default()
        },
        Agent {
            name: "test-runner".into(),
            spawn: 10800,
            finish: Some(12800),
            tools: vec![Tool::run("pytest"), Tool::look("read")],
            size: "0.6kb".into(),
            bob: 5,
            ..Agent::default()
        },
    ];
    script_activity(&mut agents, pricing);
    // Found the same way as live: each result quoted in the next agent's prompt.
    let handoffs = crate::orbit::detect_handoffs(&agents);

    let all = agents.iter().filter_map(|a| a.finish).max().unwrap_or(0) + 500;
    let events = build_events(&agents, &handoffs, all);

    Session {
        header,
        agents,
        events,
        all,
        length: LENGTH,
        live: false,
        orch_model: "opus".into(),
        orch_calls: [0, 800, 1200, 1600, 5200, 8000, 10800, 13000]
            .iter()
            .map(|&t| call(t, "claude-opus-5-5", 40, 260, 21_000, 900, pricing))
            .collect(),
        handoffs,
        ..Session::default()
    }
}

fn call(time: i64, model: &str, input: u64, output: u64, read: u64, write: u64, p: &Pricing) -> Call {
    let usage = Usage { input, output, cache_read: read, cache_write_5m: write, cache_write_1h: 0 };
    Call { time, usage, cost: p.cost(model, &usage), saved: p.cache_savings(model, &usage) }
}

const MODELS: [&str; 6] = [
    "claude-haiku-5-5",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-sonnet-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
];

/// The real tool name, its short argument, and the file it touches, for a
/// row's friendly tool label.
fn use_of(tool: &str, f: &'static str) -> (&'static str, &'static str, Option<&'static str>, bool) {
    match tool {
        "read" => ("read", f.rsplit('/').next().unwrap_or(f), Some(f), false),
        "grep" => ("grep", "fn parse_header", None, false),
        "glob" => ("glob", "src/**/*.rs", None, false),
        "web_fetch" => ("webfetch", "docs.rs", None, false),
        "web_search" => ("websearch", "braille sparkline tui", None, false),
        "pytest" => ("bash", "pytest tests/ -q", None, false),
        "edit" => ("edit", f.rsplit('/').next().unwrap_or(f), Some(f), true),
        _ => ("bash", "", None, false),
    }
}

/// One tool call plus the model call that decided on it, a beat before.
fn add_use(a: &mut Agent, i: usize, time: i64, tool: &str, f: &'static str, k: u64, p: &Pricing) {
    let (name, arg, file, edit) = use_of(tool, f);
    a.uses.push(Use { time, tool: name.into(), file: file.map(str::to_string), edit, arg: arg.into() });
    let (o, r, w) = match i {
        0 | 5 => (180 + 40 * k, 14_000, 1_200),
        1 => (900 + 150 * k, 30_000, 6_000),
        _ => (420 + 60 * k, 18_000, 2_400),
    };
    a.calls.push(call(time - 300, MODELS[i], 20, o, r, w, p));
}

/// Scripted model calls, file touches, and a red→green test run, lined up
/// with each agent's tool cycle (a tool every 1.6s after it boots) plus a few
/// extra steps. Shows off costs, the stats panel, achievements (team player:
/// three at once; speedrun; pacifist; bug hunter: the first test-runner sees
/// the failure, the second sees it pass), handoffs, and both validator verdicts.
fn script_activity(agents: &mut [Agent], p: &Pricing) {
    let files: [&[&'static str]; 6] = [
        &["src/render.rs", "src/model.rs", "src/render.rs", "README.md"],
        &["docs/api.md", "src/render.rs"],
        &["tests/test_parser.py"],
        &["src/parser.py", "tests/test_parser.py"],
        &["src/parser.py"],
        &["tests/test_parser.py"],
    ];
    for (i, a) in agents.iter_mut().enumerate() {
        a.model = pricing::family(MODELS[i]);
        let act = a.spawn + 1300;
        let finish = a.finish.unwrap_or(act);
        let mut k = 0i64;
        let mut tt = act;
        while tt < finish {
            let in_wait = a.wait.is_some_and(|(s, e)| tt >= s && tt < e);
            if !in_wait {
                let tool = a.tools[(k as usize) % a.tools.len()].name.clone();
                let f = files[i][(k as usize) % files[i].len()];
                add_use(a, i, tt, &tool, f, k as u64, p);
            }
            k += 1;
            tt = act + k * 1600;
        }
        // the final answer
        a.calls.push(call(finish - 200, MODELS[i], 20, 600, 16_000, 800, p));
    }
    // a few more steps than the tool cycle has room for
    add_use(&mut agents[3], 3, 7000, "grep", "", 1, p);
    add_use(&mut agents[4], 4, 8700, "read", "src/parser.py", 1, p);
    add_use(&mut agents[4], 4, 9700, "edit", "src/parser.py", 2, p);
    add_use(&mut agents[5], 5, 11500, "read", "tests/test_parser.py", 1, p);
    // the first test-runner's pytest (2.9s) fails, the fixer's (9.3s)
    // reproduces it, the second test-runner's (12.1s) comes back green
    agents[2].tests = vec![TestRun { time: 3100, passed: false }];
    agents[4].tests = vec![TestRun { time: 9500, passed: false }];
    agents[5].tests = vec![TestRun { time: 12400, passed: true }];
    for a in agents.iter_mut() {
        a.uses.sort_by_key(|u| u.time);
        a.calls.sort_by_key(|c| c.time);
    }

    let review = "Verdict: changes requested\n\
         1 blocking issue in src/parser.py:\n\
         - parse_header() splits on every ':' so header values with a colon get cut off\n\
         - tests/test_parser.py has no case for a value like 'Host: a:b'\n\
         Suggested fix: split(':', 1) plus a regression test.";
    let patch = "Patched parse_header() in src/parser.py to split(':', 1)\n\
         Added test_header_value_with_colon to tests/test_parser.py\n\
         pytest -x reproduced the failure before the fix.";
    let story: [(&str, String, &str); 6] = [
        (
            "map the render pipeline",
            "Map the render pipeline: which files build a frame, and in what order.".into(),
            "Render pipeline, top to bottom:\n\
             - src/render.rs  frame() builds every line\n\
             - row() draws one agent: branch, face, bar\n\
             - src/model.rs   Session / Agent / Call\n\
             - demo.rs and transcript.rs both build a Session\n\
             No edits needed; the hot path is row().\n\
             Notes:\n\
             - widths are char counts, not bytes\n\
             - colors come from theme.rs\n\
             - glyphs: braille for bars, ● for nodes\n\
             - 20fps redraw, ~300ms disk refresh",
        ),
        (
            "compare braille sparkline approaches",
            "Compare braille vs block-element sparklines for a terminal UI.".into(),
            "Two ways to draw sparklines in a terminal:\n\
             1. block elements ▁▂▃▄▅▆▇█: 8 levels, 1 col each\n\
             2. braille ⣀⣤⣶⣿: 4 levels per row, 2 cols per cell\n\
             Block elements read better at one row tall.\n\
             Braille wins for heat bars and multi-row charts.\n\
             Recommendation: blocks for sparklines,\n\
             braille for the files heatmap.",
        ),
        (
            "run the parser tests",
            "Run the parser tests and report pass or fail.".into(),
            "pytest tests/ -q: 1 failed, 13 passed\n\
             FAILED tests/test_parser.py::test_parse_header\n\
             Verdict: fail",
        ),
        (
            "review the parser change",
            "Review the parser change in src/parser.py and its tests. End with a verdict.".into(),
            review,
        ),
        (
            "fix the review findings",
            format!("Fix the blocking issue from this review, then run the tests.\n\n{review}"),
            patch,
        ),
        (
            "validate the patch",
            format!("Validate this patch: run the full test suite and report pass or fail.\n\n{patch}"),
            "pytest tests/ -q: 15 passed, 0 failed\n\
             Verdict: pass",
        ),
    ];
    for (a, (task, prompt, result)) in agents.iter_mut().zip(story) {
        a.task = task.into();
        a.prompt = prompt;
        a.result = Some(result.into());
    }
}

/// A synthetic *live* session (debug, `--synth <ms>`): one agent waiting, one
/// that hits an error at 2.5s, and one that just finished.
pub fn synth(header: String) -> Session {
    let agents = vec![
        Agent {
            name: "scout".into(),
            spawn: 0,
            finish: None,
            wait: Some((0, i64::MAX)),
            tools: vec![Tool::look("read"), Tool::run("bash")],
            live_tool: Some(1),
            size: "\u{b7}".into(),
            bob: 0,
            error: false,
            error_at: None,
            ..Agent::default()
        },
        Agent {
            name: "researcher".into(),
            spawn: 0,
            finish: None,
            wait: None,
            tools: vec![Tool::run("web_search")],
            live_tool: Some(0),
            size: "\u{b7}".into(),
            bob: 3,
            error: true,
            error_at: Some(2500),
            ..Agent::default()
        },
        Agent {
            name: "test-runner".into(),
            spawn: 0,
            finish: Some(2200),
            wait: None,
            tools: vec![Tool::run("pytest")],
            live_tool: Some(0),
            size: "1.2kb".into(),
            bob: 6,
            error: false,
            error_at: None,
            ..Agent::default()
        },
    ];
    Session {
        header,
        agents,
        events: Vec::new(),
        all: i64::MAX,
        length: 0,
        live: true,
        ..Session::default()
    }
}

/// The demo's log events (dispatches, tools, waits, results, handoffs),
/// sorted by time.
fn build_events(agents: &[Agent], handoffs: &[crate::model::Handoff], all: i64) -> Vec<Event> {
    let mut ev: Vec<Event> = Vec::new();
    for h in handoffs {
        ev.push(Event {
            time: h.time,
            label: format!("{} \u{2192} {}", agents[h.from].name, agents[h.to].name),
            detail: format!("{} {}", h.what, h.size),
            color: theme::B,
        });
    }
    for a in agents {
        let finish = a.finish.unwrap_or(0);
        let act = a.spawn + 1300;
        ev.push(Event {
            time: a.spawn,
            label: format!("orchestrator \u{2192} {}", a.name),
            detail: "dispatch".into(),
            color: theme::B,
        });
        let mut k = 0i64;
        while act + k * 1600 < finish {
            let tt = act + k * 1600;
            let in_wait = a.wait.is_some_and(|(s, e)| tt >= s && tt < e);
            if !in_wait {
                let tl = &a.tools[(k as usize) % a.tools.len()];
                ev.push(Event {
                    time: tt,
                    label: a.name.clone(),
                    detail: tl.name.clone(),
                    color: theme::G,
                });
            }
            k += 1;
        }
        if let Some((ws, we)) = a.wait {
            ev.push(Event {
                time: ws,
                label: a.name.clone(),
                detail: "? permission".into(),
                color: theme::AM,
            });
            ev.push(Event {
                time: we,
                label: a.name.clone(),
                detail: "approved".into(),
                color: theme::G,
            });
        }
        ev.push(Event {
            time: finish + 400,
            label: format!("{} \u{2192} orchestrator", a.name),
            detail: format!("result {}", a.size),
            color: theme::B,
        });
    }
    ev.push(Event {
        time: all,
        label: "orchestrator".into(),
        detail: "all agents done \u{2713}".into(),
        color: theme::ac(),
    });
    ev.sort_by_key(|e| e.time);
    ev
}

// ---- mixed demo (`--demo --source all`) -------------------------------------

/// One scripted tool call: (row label, look or run, tool, short arg, file, edits).
type Step = (&'static str, ToolKind, &'static str, &'static str, Option<&'static str>, bool);

/// One scripted agent for the mixed demo.
struct Spec {
    name: &'static str,
    tag: &'static str,
    model: &'static str,
    spawn: i64,
    finish: i64,
    steps: &'static [Step],
    task: &'static str,
    prompt: &'static str,
    result: &'static str,
    tests: &'static [(i64, bool)],
}

const LOOK: ToolKind = ToolKind::Look;
const RUN: ToolKind = ToolKind::Run;

/// A scripted orchestrated session: each agent works through its steps (one
/// every 1.6s once it boots) with a model call a beat before each, then
/// answers. Handoffs are detected the same way as live.
fn scripted(specs: &[Spec], orch_model: &str, p: &Pricing) -> Session {
    let mut agents: Vec<Agent> = specs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut a = Agent {
                name: s.name.into(),
                tag: s.tag.into(),
                model: pricing::family(s.model),
                spawn: s.spawn,
                finish: Some(s.finish),
                tools: s.steps.iter().map(|st| Tool { name: st.0.into(), kind: st.1 }).collect(),
                size: format!("{:.1}kb", s.result.len() as f64 / 1024.0 * 4.0),
                bob: i as i64 * 4 + 1,
                task: s.task.into(),
                prompt: s.prompt.into(),
                result: Some(s.result.into()),
                tests: s.tests.iter().map(|&(time, passed)| TestRun { time, passed }).collect(),
                ..Agent::default()
            };
            let mut tt = s.spawn + 1300;
            let mut k = 0usize;
            while tt < s.finish {
                let (_, _, tool, arg, file, edit) = s.steps[k % s.steps.len()];
                a.uses.push(Use { time: tt, tool: tool.into(), file: file.map(str::to_string), edit, arg: arg.into() });
                a.calls.push(call(tt - 300, s.model, 20, 300 + 80 * k as u64, 16_000, 1_500, p));
                k += 1;
                tt = s.spawn + 1300 + k as i64 * 1600;
            }
            a.calls.push(call(s.finish - 200, s.model, 20, 700, 18_000, 900, p));
            a
        })
        .collect();
    for a in &mut agents {
        a.uses.sort_by_key(|u| u.time);
    }
    let handoffs = crate::orbit::detect_handoffs(&agents);
    let all = agents.iter().filter_map(|a| a.finish).max().unwrap_or(0) + 500;
    let events = build_events(&agents, &handoffs, all);
    let orch_calls = std::iter::once(0)
        .chain(specs.iter().map(|s| s.spawn))
        .chain([all - 500])
        .map(|t| call(t, orch_model, 40, 240, 20_000, 800, p))
        .collect();
    Session {
        agents,
        events,
        all,
        length: LENGTH,
        live: false,
        orch_model: pricing::family(orch_model),
        orch_calls,
        handoffs,
        ..Session::default()
    }
}

const REVIEW: &str = "Verdict: changes requested\n\
    - retry() in src/client.rs never backs off: a 429 loops at full speed\n\
    - no test covers a server that answers 429 twice\n\
    Suggested fix: exponential backoff, capped at 8s, plus a test.";

const AUDIT: &str = "Test audit for src/client.rs:\n\
    - 14 tests, 2 slow (network), 0 flaky in the last 20 runs\n\
    - retry paths are covered only for 500s, not 429s\n\
    - suggest a fake server fixture in tests/common/mod.rs";

/// The mixed demo: Claude Code and Codex in the same project at once. Claude
/// orchestrates a scout, a reviewer and a fixer (the review is handed to the
/// fixer); Codex runs two subagents of its own, a little later.
pub fn mixed(header: String, p: &Pricing) -> Session {
    let claude = scripted(
        &[
            Spec {
                name: "scout",
                tag: "",
                model: "claude-haiku-5-5",
                spawn: 600,
                finish: 4_200,
                steps: &[
                    ("read", LOOK, "read", "client.rs", Some("src/client.rs"), false),
                    ("grep", LOOK, "grep", "fn retry", None, false),
                    ("read", LOOK, "read", "lib.rs", Some("src/lib.rs"), false),
                ],
                task: "map the HTTP client",
                prompt: "Map the HTTP client: where requests are built, sent and retried.",
                result: "src/client.rs builds and sends requests; retry() wraps send().\n\
                    src/lib.rs re-exports Client. No backoff anywhere.",
                tests: &[],
            },
            Spec {
                name: "reviewer",
                tag: "",
                model: "claude-sonnet-5-5",
                spawn: 1_000,
                finish: 6_400,
                steps: &[
                    ("read", LOOK, "read", "client.rs", Some("src/client.rs"), false),
                    ("grep", LOOK, "grep", "429", None, false),
                    ("read", LOOK, "read", "client_test.rs", Some("tests/client_test.rs"), false),
                ],
                task: "review the retry logic",
                prompt: "Review the retry logic in src/client.rs. End with a verdict.",
                result: REVIEW,
                tests: &[],
            },
            Spec {
                name: "fixer",
                tag: "",
                model: "claude-opus-5-5",
                spawn: 7_000,
                finish: 12_200,
                steps: &[
                    ("edit", RUN, "edit", "client.rs", Some("src/client.rs"), true),
                    ("cargo", RUN, "bash", "cargo test retry", None, false),
                    ("edit", RUN, "edit", "client_test.rs", Some("tests/client_test.rs"), true),
                ],
                task: "fix the review findings",
                prompt: concat!(
                    "Fix the blocking issues from this review, then run the tests.\n\n",
                    "Verdict: changes requested\n",
                    "- retry() in src/client.rs never backs off: a 429 loops at full speed\n",
                    "- no test covers a server that answers 429 twice\n",
                    "Suggested fix: exponential backoff, capped at 8s, plus a test."
                ),
                result: "Added exponential backoff (capped at 8s) to retry().\n\
                    New test: retries_twice_on_429. cargo test: 15 passed.",
                tests: &[(10_100, true)],
            },
        ],
        "claude-opus-5-5",
        p,
    );
    let codex = scripted(
        &[
            Spec {
                name: "socrates",
                tag: "repo_overview",
                model: "gpt-6.1-sol",
                spawn: 2_400,
                finish: 7_800,
                steps: &[
                    ("grep", LOOK, "grep", "rg -n retry", None, false),
                    ("read", LOOK, "read", "client.rs", Some("src/client.rs"), false),
                    ("ls", LOOK, "ls", "ls src", None, false),
                ],
                task: "repo_overview",
                prompt: "Give me an overview of the repo: crates, entry points, tests.",
                result: "One crate. Entry: src/lib.rs. HTTP in src/client.rs.\n\
                    Tests in tests/, one fixture module.",
                tests: &[],
            },
            Spec {
                name: "hume",
                tag: "test_audit",
                model: "gpt-6-luna",
                spawn: 3_000,
                finish: 10_600,
                steps: &[
                    ("cargo", RUN, "bash", "cargo test -- --list", None, false),
                    ("grep", LOOK, "grep", "rg -n test", None, false),
                    ("edit", RUN, "edit", "mod.rs", Some("tests/common/mod.rs"), true),
                ],
                task: "test_audit",
                prompt: "Audit the tests around src/client.rs: coverage gaps, slow or flaky tests.",
                result: AUDIT,
                tests: &[(4_400, true)],
            },
        ],
        "gpt-6.1-sol",
        p,
    );
    crate::mixed::merge(header, vec![("claude".into(), claude), ("codex".into(), codex)])
}
