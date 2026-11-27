//! Routing accuracy of the built-in rules against a hosted decision model, on a labelled set. Ignored by default: the hosted run
//! calls the Decisions API.
//!
//! ```bash
//! PLANNER_API_KEY=… cargo test -p dfae-decision --test routing_eval -- --ignored --nocapture
//! ```
//!
//! Each label is what a correct router answers. The set mixes the request shapes the engine sees: lookups, edits, questions about
//! files, builds, small talk, explanations, and fresh data.

use dfae_core::*;
use dfae_decision::{Calibrator, Decider, HostedDecisions};
use std::sync::Arc;
use std::time::Duration;

/// (request, needs_tools, changes, asks, smalltalk, fresh_data)
const CASES: &[(&str, bool, bool, bool, bool, bool)] = &[
    ("hi", false, false, false, true, false),
    ("thanks, that worked!", false, false, false, true, false),
    ("how are you today?", false, false, false, true, false),
    ("Explain how a mutex differs from a semaphore", false, false, true, false, false),
    ("Write a haiku about autumn", false, false, false, false, false),
    ("What is the capital of France?", false, false, true, false, false),
    ("what is 12*(3+4)", true, false, true, false, false),
    ("list the files in the current directory", true, false, false, false, false),
    ("show me notes.txt", true, false, false, false, false),
    ("Read changelog.md and tell me what changed in version 2", true, false, true, false, false),
    ("Which files mention TODO, and what is the first one about?", true, false, true, false, false),
    ("how many lines does src/main.rs have?", true, false, true, false, false),
    ("In config.toml change the port from 8080 to 9090", true, true, false, false, false),
    ("create a landing page about earth in the site directory", true, true, false, false, false),
    ("Create README.md for a tool called Brewster and a .gitignore for Python", true, true, false, false, false),
    ("delete the build directory", true, true, false, false, false),
    ("rename the function foo to bar in src/lib.rs", true, true, false, false, false),
    ("build a todo app with Next.js and serve it on a port", true, true, false, false, false),
    ("run the tests and fix whatever fails", true, true, false, false, false),
    ("install ffmpeg and convert video.mov to mp4", true, true, false, false, false),
    ("start a python http server in the site folder", true, true, false, false, false),
    ("what's the weather in Paris right now?", true, false, true, false, true),
    ("what is the current price of bitcoin?", true, false, true, false, true),
    ("latest news about the Rust programming language", true, false, true, false, true),
    ("what is the latest version of Node.js?", true, false, true, false, true),
    ("check disk usage on this machine", true, false, false, false, false),
    ("is port 8000 in use?", true, false, true, false, false),
    ("summarise the README", true, false, true, false, false),
    ("compare a.csv and b.csv and tell me which rows differ", true, false, true, false, false),
    ("write a python script that prints the first 20 primes and run it", true, true, false, false, false),
    ("fetch https://example.com and tell me its title", true, false, true, false, false),
    ("find all python files in the repo", true, false, false, false, false),
    ("remember that my server is db01.internal", true, true, false, false, false),
    ("what did we talk about earlier?", false, false, true, false, false),
    ("make the page background blue", true, true, false, false, false),
    ("why does my build fail? the log is in build.log", true, false, true, false, false),
];

/// Held out: written and labelled before any run, never used to tune a question's wording.
const HELDOUT: &[(&str, bool, bool, bool, bool, bool)] = &[
    ("good morning!", false, false, false, true, false),
    ("bye for now", false, false, false, true, false),
    ("What does HTTP 418 mean?", false, false, true, false, false),
    ("give me three name ideas for a bakery", false, false, false, false, false),
    ("translate 'good night' into Spanish", false, false, false, false, false),
    ("what is 2 to the power of 40", true, false, true, false, false),
    ("show the last 20 lines of server.log", true, false, false, false, false),
    ("print the contents of package.json", true, false, false, false, false),
    ("which version of python is installed here?", true, false, true, false, false),
    ("what does the function parse_args in cli.py do?", true, false, true, false, false),
    ("how many TODO comments are in the src folder?", true, false, true, false, false),
    ("are there any failing tests?", true, false, true, false, false),
    ("add a dark mode toggle to site/index.html", true, true, false, false, false),
    ("create a folder called reports", true, true, false, false, false),
    ("move all .png files into images/", true, true, false, false, false),
    ("write unit tests for utils.py and run them", true, true, false, false, false),
    ("set up a FastAPI hello world and run it on port 8002", true, true, false, false, false),
    ("upgrade the dependencies in requirements.txt", true, true, false, false, false),
    ("kill the process listening on port 8001", true, true, false, false, false),
    ("what's the USD to EUR exchange rate today?", true, false, true, false, true),
    ("who won the last Formula 1 race?", true, false, true, false, true),
    ("is it going to rain in London tomorrow?", true, false, true, false, true),
    ("show me the running processes", true, false, false, false, false),
    ("how much free memory does this machine have?", true, false, true, false, false),
    ("search the web for the Rust 2024 edition changes and summarise them", true, false, true, false, true),
    ("download https://example.com/data.csv into data/", true, true, false, false, false),
    ("tell me what's in the docs folder", true, false, true, false, false),
    ("explain the difference between TCP and UDP", false, false, true, false, false),
    ("fix the typo in README.md: 'recieve' should be 'receive'", true, true, false, false, false),
    ("what time is it?", true, false, true, false, false),
];

async fn score(d: &Decider, label: &str) -> (usize, usize, Vec<String>) {
    let (mut right, mut total, mut misses) = (0, 0, Vec::new());
    let mut fallback = 0;
    let set = if label.ends_with("heldout") { HELDOUT } else { CASES };
    for (req, tools, changes, asks, small, fresh) in set {
        let out = d.route_task(DecisionState { request: (*req).into(), ..Default::default() }).await;
        if out.record.provider.starts_with("local") && !label.starts_with("rules") {
            fallback += 1;
        }
        let r = out.value;
        for (name, got, want) in [
            ("needs_tools", r.needs_tools, *tools),
            ("changes", r.changes, *changes),
            ("asks", r.asks, *asks),
            ("smalltalk", r.smalltalk.is_some(), *small),
            ("fresh_data", r.fresh_data, *fresh),
        ] {
            total += 1;
            if got == want {
                right += 1;
            } else {
                misses.push(format!("{name}={got} (want {want}): {req}"));
            }
        }
    }
    if fallback > 0 {
        misses.push(format!("({fallback} decisions fell back to the rules)"));
    }
    (right, total, misses)
}

#[tokio::test]
#[ignore = "calls the hosted Decisions API: see the module docs"]
async fn rules_and_the_hosted_model_on_labelled_routing() {
    let cfg = Config::from_env();
    let rules = Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone());
    let h = cfg.decision.hosted.clone().expect("no hosted decisions configured (PLANNER_API_KEY on Vercel AI Gateway)");
    let hosted = HostedDecisions::new(&h.base_url, &h.model, h.api_key.clone(), Duration::from_millis(h.timeout_ms)).unwrap();
    let hosted = Decider::new(Some(Arc::new(hosted)), Arc::new(Calibrator::new()), cfg.thresholds.clone());
    for (label, d) in [("rules", &rules), ("hosted", &hosted), ("rules heldout", &rules), ("hosted heldout", &hosted)] {
        let (right, total, misses) = score(d, label).await;
        eprintln!("\n{label}: {right}/{total} = {:.1}%", 100.0 * right as f64 / total as f64);
        for m in misses {
            eprintln!("  miss {m}");
        }
    }
}
