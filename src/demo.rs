//! The scripted demo session — a direct port of the `A` array and `EV`
//! derivation in agentflow-mockup.html. Replays on a loop.

use crate::model::{Agent, Call, Event, Session, TestRun, Tool, Use, Usage};
use crate::pricing::{self, Pricing};
use crate::theme;

/// Build the demo session (same timeline, faces, sizes, and log as the mockup).
/// `header` is the full header string, derived from the real watched directory.
pub fn session(header: String, pricing: &Pricing) -> Session {
    let agents = vec![
        Agent {
            name: "scout".into(),
            spawn: 1000,
            finish: Some(9300),
            wait: None,
            tools: vec![Tool::look("read"), Tool::look("grep"), Tool::look("glob")],
            live_tool: None,
            size: "2.1kb".into(),
            bob: 0,
            error: false,
            error_at: None,
            ..Agent::default()
        },
        Agent {
            name: "researcher".into(),
            spawn: 1800,
            finish: Some(10800),
            wait: Some((5600, 7300)),
            tools: vec![
                Tool::run("web_fetch"),
                Tool::look("read"),
                Tool::run("web_search"),
            ],
            live_tool: None,
            size: "4.8kb".into(),
            bob: 3,
            error: false,
            error_at: None,
            ..Agent::default()
        },
        Agent {
            name: "test-runner".into(),
            spawn: 2600,
            finish: Some(7600),
            wait: None,
            tools: vec![Tool::run("bash"), Tool::run("pytest"), Tool::look("read")],
            live_tool: None,
            size: "1.2kb".into(),
            bob: 6,
            error: false,
            error_at: None,
            ..Agent::default()
        },
    ];
    let mut agents = agents;
    script_activity(&mut agents, pricing);

    let all = agents.iter().filter_map(|a| a.finish).max().unwrap_or(0) + 500;
    let events = build_events(&agents, all);

    Session {
        header,
        agents,
        events,
        all,
        length: 14500,
        live: false,
        orch_model: "opus".into(),
        orch_calls: [0, 1000, 1800, 2600, 11300]
            .iter()
            .map(|&t| call(t, "claude-opus-5-5", 40, 260, 21_000, 900, pricing))
            .collect(),
        ..Session::default()
    }
}

fn call(time: i64, model: &str, input: u64, output: u64, read: u64, write: u64, p: &Pricing) -> Call {
    let usage = Usage { input, output, cache_read: read, cache_write_5m: write, cache_write_1h: 0 };
    Call { time, usage, cost: p.cost(model, &usage), saved: p.cache_savings(model, &usage) }
}

/// Scripted model calls, file touches, and one red→green test run, lined up
/// with each agent's tool cycle (a tool every 1.6s after it boots). Shows off
/// costs, the stats panel, and three achievements: team player (3 at once),
/// bug hunter (pytest fails, then passes), pacifist (scout makes no edits).
fn script_activity(agents: &mut [Agent], p: &Pricing) {
    let models = ["claude-haiku-5-5", "claude-opus-5-5", "claude-sonnet-5-5"];
    let files: [&[&str]; 3] = [
        &["src/render.rs", "src/model.rs", "src/render.rs", "README.md"],
        &["docs/api.md", "src/render.rs"],
        &["tests/test_parser.py", "src/parser.py"],
    ];
    for (i, a) in agents.iter_mut().enumerate() {
        a.model = pricing::family(models[i]);
        let act = a.spawn + 1300;
        let finish = a.finish.unwrap_or(act);
        let mut k = 0i64;
        let mut tt = act;
        while tt < finish {
            let in_wait = a.wait.is_some_and(|(s, e)| tt >= s && tt < e);
            if !in_wait {
                let tool = &a.tools[(k as usize) % a.tools.len()];
                let f = files[i][(k as usize) % files[i].len()];
                // the row shows a friendly label; stats and the detail view
                // get the real tool name and its argument
                let (name, arg, file) = match tool.name.as_str() {
                    "read" => ("read", f.rsplit('/').next().unwrap_or(f), Some(f)),
                    "grep" => ("grep", "fn frame", None),
                    "glob" => ("glob", "src/**/*.rs", None),
                    "web_fetch" => ("webfetch", "docs.rs", None),
                    "web_search" => ("websearch", "braille sparkline tui", None),
                    "pytest" => ("bash", "pytest tests/ -q", None),
                    "bash" => ("bash", "pytest -x", None),
                    other => (other, "", None),
                };
                a.uses.push(Use {
                    time: tt,
                    tool: name.into(),
                    file: file.map(str::to_string),
                    edit: false,
                    arg: arg.into(),
                });
                // the call that decided on this tool, a beat before it
                let (o, r, w) = match i {
                    0 => (180 + 40 * k as u64, 14_000, 1_200),
                    1 => (900 + 150 * k as u64, 30_000, 6_000),
                    _ => (420 + 60 * k as u64, 18_000, 2_400),
                };
                a.calls.push(call(tt - 300, models[i], 20, o, r, w, p));
            }
            k += 1;
            tt = act + k * 1600;
        }
        // the final answer
        a.calls.push(call(finish - 200, models[i], 20, 600, 16_000, 800, p));
    }
    // test-runner: `bash` runs pytest red, a quick fix, then pytest goes green
    let tr = &mut agents[2];
    tr.tests = vec![
        TestRun { time: tr.spawn + 1300, passed: false },
        TestRun { time: tr.spawn + 2900, passed: true },
    ];
    tr.uses.push(Use {
        time: tr.spawn + 2200,
        tool: "edit".into(),
        file: Some("src/parser.py".into()),
        edit: true,
        arg: "parser.py".into(),
    });
    tr.uses.sort_by_key(|u| u.time);

    let story: [(&str, &str); 3] = [
        (
            "map the render pipeline",
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
            "Two ways to draw sparklines in a terminal:\n\
             1. block elements ▁▂▃▄▅▆▇█: 8 levels, 1 col each\n\
             2. braille ⣀⣤⣶⣿: 4 levels per row, 2 cols per cell\n\
             Block elements read better at one row tall.\n\
             Braille wins for heat bars and multi-row charts.\n\
             Recommendation: blocks for sparklines,\n\
             braille for the files heatmap.",
        ),
        (
            "run parser tests and fix failures",
            "pytest: 1 failed → fixed → 14 passed\n\
             Cause: parse_header() split on ':' once too often.\n\
             Fix: src/parser.py, split(':', 1).\n\
             All parser tests green.",
        ),
    ];
    for (a, (task, result)) in agents.iter_mut().zip(story) {
        a.task = task.into();
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

/// Port of the mockup's `EV` construction, then sorted by time.
fn build_events(agents: &[Agent], all: i64) -> Vec<Event> {
    let mut ev: Vec<Event> = Vec::new();
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
