//! The scripted demo session — a direct port of the `A` array and `EV`
//! derivation in agentflow-mockup.html. Replays on a loop.

use crate::model::{Agent, Event, Session, Tool};
use crate::theme;

/// Build the demo session (same timeline, faces, sizes, and log as the mockup).
/// `header` is the full header string, derived from the real watched directory.
pub fn session(header: String) -> Session {
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
        },
    ];

    let all = agents.iter().filter_map(|a| a.finish).max().unwrap_or(0) + 500;
    let events = build_events(&agents, all);

    Session {
        header,
        agents,
        events,
        all,
        length: 14500,
        live: false,
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
        },
    ];
    Session {
        header,
        agents,
        events: Vec::new(),
        all: i64::MAX,
        length: 0,
        live: true,
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
        color: theme::AC,
    });
    ev.sort_by_key(|e| e.time);
    ev
}
