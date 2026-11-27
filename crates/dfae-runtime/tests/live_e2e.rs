//! Harder end-to-end tasks against the real system: the real bootstrap (`build_engine`), the container's sandbox policy
//! (`config/policy.toml`), automatic routing by the configured decision provider, the real planner and worker. Each case has an
//! oracle that checks the outcome (files, exit codes, served pages, answers), not the steps taken. Ignored by default.
//!
//! ```bash
//! PLANNER_API_KEY=… cargo test -p dfae-runtime --test live_e2e -- --ignored --nocapture --test-threads=1
//! LIVE_CASES=bugfix,csv …   # only the cases whose name contains one of the words
//! LIVE_RUNS=3 …             # each case three times
//! ```
//!
//! Only run it inside a container: the policy is the sandbox one (a full bash in the workspace).

use dfae_core::*;
use dfae_runtime::build_engine;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

struct Case {
    name: &'static str,
    goal: &'static str,
    /// Follow-up requests in the same workspace, after the goal (the oracle checks the state after the last). One that starts with
    /// `new:` is asked in a new session (what it needs comes from long-term memory, not the conversation).
    then: &'static [&'static str],
    /// Run before the follow-ups (to take away what memory has to replace).
    between: Option<fn(&Path)>,
    files: fn(&Path),
    check: fn(&Path, &TaskRecord) -> Result<(), String>,
}

fn write(ws: &Path, p: &str, t: &str) {
    let f = ws.join(p);
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(f, t).unwrap();
}

fn run(ws: &Path, cmd: &str) -> (bool, String) {
    let o = Command::new("bash").arg("-c").arg(cmd).current_dir(ws).output().unwrap();
    (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn answer(t: &TaskRecord) -> String {
    t.final_answer.clone().unwrap_or_default()
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "bugfix with failing tests",
            then: &[],
            between: None,
            goal: "The tests in test_stats.py fail. Fix stats.py so that all of them pass.",
            files: |ws| {
                write(ws, "stats.py", "def median(xs):\n    xs.sort()\n    return xs[len(xs) // 2]\n\n\ndef mean(xs):\n    return sum(xs) / len(xs)\n");
                write(
                    ws,
                    "test_stats.py",
                    "import pytest\nfrom stats import median, mean\n\n\ndef test_median_odd():\n    assert median([3, 1, 2]) == 2\n\n\ndef test_median_even():\n    assert median([4, 1, 3, 2]) == 2.5\n\n\ndef test_median_does_not_mutate():\n    xs = [3, 1, 2]\n    median(xs)\n    assert xs == [3, 1, 2]\n\n\ndef test_median_empty():\n    with pytest.raises(ValueError):\n        median([])\n\n\ndef test_mean_empty():\n    with pytest.raises(ValueError):\n        mean([])\n",
                );
            },
            check: |ws, _| {
                let (ok, out) = run(ws, "python3 -m pytest -q test_stats.py 2>&1 | tail -5");
                let tests = std::fs::read_to_string(ws.join("test_stats.py")).unwrap_or_default();
                if !tests.contains("test_median_empty") {
                    return Err("the tests were changed".into());
                }
                if ok && out.contains("5 passed") { Ok(()) } else { Err(format!("pytest: {out}")) }
            },
        },
        Case {
            name: "cli with tests",
            then: &[],
            between: None,
            goal: "Write a Python command-line tool wordcount.py that prints the number of words in the file given as its argument, \
                   write pytest tests for it in test_wordcount.py, and run the tests.",
            files: |ws| write(ws, "sample.txt", "the quick brown fox\njumps over the lazy dog\n"),
            check: |ws, _| {
                let (ok, out) = run(ws, "python3 wordcount.py sample.txt");
                if !ok || !out.contains('9') {
                    return Err(format!("wordcount.py sample.txt printed: {out}"));
                }
                let (ok, out) = run(ws, "python3 -m pytest -q test_wordcount.py 2>&1 | tail -3");
                if ok && out.contains("passed") { Ok(()) } else { Err(format!("pytest: {out}")) }
            },
        },
        Case {
            name: "csv analysis",
            then: &[],
            between: None,
            goal: "sales.csv has one row per order. Which region had the highest total revenue? Also write the total revenue per region \
                   to totals.csv with the columns region,revenue.",
            files: |ws| {
                let mut s = String::from("order_id,region,units,unit_price\n");
                let regions = ["north", "south", "east", "west"];
                for i in 0..200u32 {
                    let r = regions[(i * 7 % 4) as usize];
                    let units = 1 + (i * 13) % 9;
                    let price = 5 + (i * 31) % 40;
                    s.push_str(&format!("{},{},{},{}\n", 1000 + i, r, units, price));
                }
                write(ws, "sales.csv", &s);
            },
            check: |ws, t| {
                // the truth, computed here
                let data = std::fs::read_to_string(ws.join("sales.csv")).unwrap();
                let mut totals = std::collections::BTreeMap::<String, u64>::new();
                for l in data.lines().skip(1) {
                    let f: Vec<&str> = l.split(',').collect();
                    *totals.entry(f[1].to_string()).or_default() += f[2].parse::<u64>().unwrap() * f[3].parse::<u64>().unwrap();
                }
                let best = totals.iter().max_by_key(|(_, v)| **v).map(|(k, _)| k.clone()).unwrap();
                let a = answer(t).to_lowercase();
                if !a.contains(&best) {
                    return Err(format!("the answer does not name {best} ({totals:?}): {a}"));
                }
                let out = std::fs::read_to_string(ws.join("totals.csv")).map_err(|e| format!("totals.csv: {e}"))?;
                for (r, v) in &totals {
                    let line_ok = out.lines().any(|l| {
                        let l = l.to_lowercase();
                        l.starts_with(r.as_str()) && l.split(',').nth(1).and_then(|x| x.trim().parse::<f64>().ok()).is_some_and(|x| (x - *v as f64).abs() < 0.5)
                    });
                    if !line_ok {
                        return Err(format!("totals.csv has no correct line for {r}={v}: {out}"));
                    }
                }
                Ok(())
            },
        },
        Case {
            name: "rename across a js project",
            then: &[],
            between: None,
            goal: "Rename the function calcTotal to computeTotal everywhere in src/, and make sure `node test.js` still passes.",
            files: |ws| {
                write(
                    ws,
                    "src/cart.js",
                    "function calcTotal(items) {\n  return items.reduce((s, i) => s + i.price * i.qty, 0);\n}\nmodule.exports = { calcTotal };\n",
                );
                write(
                    ws,
                    "src/checkout.js",
                    "const { calcTotal } = require('./cart');\nfunction checkout(items) {\n  const total = calcTotal(items);\n  return { total, tax: Math.round(total * 0.2 * 100) / 100 };\n}\nmodule.exports = { checkout };\n",
                );
                write(
                    ws,
                    "test.js",
                    "const assert = require('assert');\nconst { checkout } = require('./src/checkout');\nassert.deepStrictEqual(checkout([{ price: 10, qty: 2 }, { price: 5, qty: 1 }]), { total: 25, tax: 5 });\nconsole.log('ok');\n",
                );
            },
            check: |ws, _| {
                let (_, grep) = run(ws, "grep -rn calcTotal src/ || true");
                if !grep.trim().is_empty() {
                    return Err(format!("calcTotal is still used: {grep}"));
                }
                let (_, grep) = run(ws, "grep -rln computeTotal src/ | wc -l");
                if grep.trim() != "2" {
                    return Err(format!("computeTotal is in {} files", grep.trim()));
                }
                let (ok, out) = run(ws, "node test.js");
                if ok && out.contains("ok") { Ok(()) } else { Err(format!("node test.js: {out}")) }
            },
        },
        Case {
            name: "static site served",
            then: &[],
            between: None,
            goal: "Create a small static website for a bakery called Crumb & Co in the site directory, with a home page and an about page \
                   linked from each other, and serve it on a free port from 8000 to 8010. Tell me the address.",
            files: |_| {},
            check: |ws, t| {
                let a = answer(t);
                let port = (8000..=8010).find(|p| a.contains(&format!(":{p}"))).ok_or_else(|| format!("no address in the answer: {a}"))?;
                let (ok, home) = run(ws, &format!("curl -fsS http://127.0.0.1:{port}/"));
                if !ok || !home.contains("Crumb") {
                    return Err(format!("the home page on {port}: {}", home.chars().take(200).collect::<String>()));
                }
                let lower = home.to_lowercase();
                let href = lower.split("href=\"").skip(1).map(|h| h.split('"').next().unwrap_or("")).find(|h| h.contains("about"));
                let href = href.ok_or("the home page does not link to an about page")?;
                let (ok, about) = run(ws, &format!("curl -fsS http://127.0.0.1:{port}/{}", href.trim_start_matches('/')));
                // a second page about the bakery ("Our Story — Crumb & Co" is an about page without the word)
                if ok && about.contains("Crumb") && about != home { Ok(()) } else { Err(format!("the about page ({href}): {about}")) }
            },
        },
        Case {
            name: "question about the code",
            then: &[],
            between: None,
            goal: "What does the function apply_discount in shop/pricing.py do, and what does it return for a price of 80 with the code SAVE10?",
            files: |ws| {
                write(
                    ws,
                    "shop/pricing.py",
                    "CODES = {\"SAVE10\": 0.10, \"HALF\": 0.5}\n\n\ndef apply_discount(price, code=None):\n    \"\"\"Price after a discount code; unknown codes are ignored; never below 1.\"\"\"\n    rate = CODES.get((code or \"\").upper(), 0)\n    return max(1, round(price * (1 - rate), 2))\n",
                );
                write(ws, "shop/__init__.py", "");
            },
            check: |_, t| {
                let a = answer(t);
                if a.contains("72") { Ok(()) } else { Err(format!("the answer does not say 72: {a}")) }
            },
        },
        Case {
            name: "web lookup",
            goal: "What is the latest stable release of Python? Look it up on the web and give me the version number.",
            then: &[],
            between: None,
            files: |_| {},
            check: |_, t| {
                let a = answer(t);
                let web = t
                    .plan
                    .as_ref()
                    .is_some_and(|p| p.steps.iter().any(|s| matches!(s.tool.as_deref(), Some("web_search" | "http_get")) && s.status == StepStatus::Succeeded));
                let version = regex::Regex::new(r"\b3\.1[3-9]\.\d+\b").unwrap();
                if web && version.is_match(&a) { Ok(()) } else { Err(format!("web used: {web}; answer: {a}")) }
            },
        },
        Case {
            name: "follow-up in a session",
            goal: "Create a one-page website for a florist called Petal Lane in the shop directory.",
            then: &["Now add a contact page to it, with an address and opening hours, and link it from the home page."],
            between: None,
            files: |_| {},
            check: |ws, _| {
                let (_, found) = run(ws, "ls shop");
                let contact = found.split_whitespace().find(|f| f.to_lowercase().contains("contact")).ok_or(format!("no contact page in shop/: {found}"))?;
                let c = std::fs::read_to_string(ws.join("shop").join(contact)).unwrap_or_default();
                let home = std::fs::read_to_string(ws.join("shop/index.html")).map_err(|e| format!("shop/index.html: {e}"))?;
                if !home.contains(contact) {
                    return Err(format!("the home page does not link to {contact}"));
                }
                if c.contains("Petal Lane") && c.to_lowercase().contains("hours") {
                    Ok(())
                } else {
                    Err(format!("{contact}: {}", c.chars().take(300).collect::<String>()))
                }
            },
        },
        Case {
            name: "remembered across sessions",
            goal: "Create config/billing.toml for our billing service: it runs on the host billing-7.internal on port 7421.",
            then: &["new: Where does the service that handles our invoices live — which machine, and which port?"],
            between: Some(|ws| {
                let _ = std::fs::remove_dir_all(ws.join("config"));
            }),
            files: |_| {},
            check: |_, t| {
                let a = answer(t);
                if a.contains("7421") && a.contains("billing-7") { Ok(()) } else { Err(format!("the answer does not recall it: {a}")) }
            },
        },
        Case {
            name: "nextjs scaffold and build",
            goal: "Create a Next.js app in the directory notes-app whose home page lists three example notes, and make sure `npm run build` succeeds.",
            then: &[],
            between: None,
            files: |_| {},
            check: |ws, _| {
                let pkg = std::fs::read_to_string(ws.join("notes-app/package.json")).map_err(|e| format!("notes-app/package.json: {e}"))?;
                if !pkg.contains("\"next\"") {
                    return Err("not a Next.js project".into());
                }
                let (ok, out) = run(&ws.join("notes-app"), "npm run build 2>&1 | tail -15");
                if ok && !out.to_lowercase().contains("error") { Ok(()) } else { Err(format!("npm run build: {out}")) }
            },
        },
    ]
}

/// Stop servers a case started (they keep the ports for the next one).
fn free_ports() {
    for p in 8000..=8010 {
        let _ = Command::new("fuser").arg("-k").arg(format!("{p}/tcp")).output();
    }
}

#[tokio::test]
#[ignore = "calls the hosted models and runs a sandbox shell: see the module docs"]
async fn harder_tasks_end_to_end() {
    let only: Vec<String> = std::env::var("LIVE_CASES").map(|v| v.split(',').map(|w| w.trim().to_lowercase()).collect()).unwrap_or_default();
    let runs: usize = std::env::var("LIVE_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let policy = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/policy.toml");
    let mut failures = Vec::new();
    let mut total = 0;
    let mut cost = 0.0;
    for c in cases().into_iter().filter(|c| only.is_empty() || only.iter().any(|w| c.name.contains(w.as_str()))) {
        for run_no in 0..runs {
            free_ports();
            total += 1;
            let ws = tempfile::tempdir().unwrap();
            (c.files)(ws.path());
            let mut cfg = Config::from_env();
            cfg.workspace = ws.path().to_path_buf();
            cfg.policy_path = Some(policy.into());
            cfg.approvals = ApprovalMode::Never;
            cfg.database_url = None;
            cfg.mcp.auto = false;
            let e = build_engine(cfg).await.expect("engine");
            let started = std::time::Instant::now();
            let session = format!("e2e-{}-{run_no}", c.name.replace(' ', "-"));
            let id = e.submit(TaskRequest { goal: c.goal.into(), session_id: Some(session.clone()), ..Default::default() }).await.unwrap();
            let mut t = e.wait(&id, Duration::from_secs(900)).await.expect("the task did not finish in 15 minutes");
            if let Some(b) = c.between {
                // memory is written after a task finishes: give it a moment
                tokio::time::sleep(Duration::from_secs(8)).await;
                b(ws.path());
            }
            for follow in c.then {
                if t.status != TaskStatus::Completed {
                    break;
                }
                let first = t.ledger.summary().cost_usd;
                let (follow, session) = match follow.strip_prefix("new:") {
                    Some(f) => (f.trim(), format!("{session}-new")),
                    None => (*follow, session.clone()),
                };
                let id = e.submit(TaskRequest { goal: follow.into(), session_id: Some(session), ..Default::default() }).await.unwrap();
                t = e.wait(&id, Duration::from_secs(900)).await.expect("the follow-up did not finish in 15 minutes");
                cost += first;
                eprintln!("  (follow-up: {follow})");
            }
            let s = t.ledger.summary();
            cost += s.cost_usd;
            eprintln!(
                "\n=== {} (run {}) — {:?} {:?} in {}s, mode {:?}, replans {}, verification {:?}",
                c.name,
                run_no + 1,
                t.status,
                t.plan.as_ref().map(|p| p.source),
                started.elapsed().as_secs(),
                t.mode,
                t.usage.replans,
                t.verification.as_ref().map(|v| v.status)
            );
            if let Some(p) = &t.plan {
                for st in &p.steps {
                    eprintln!(
                        "  {:<6} {:<10} {:<9} how={:<14} {}{}",
                        st.id,
                        st.tool.as_deref().unwrap_or("reasoning"),
                        format!("{:?}", st.status),
                        st.how.as_deref().unwrap_or("-"),
                        st.error.as_deref().map(|e| format!("ERROR {} ", tokens::truncate_tokens(e, 40))).unwrap_or_default(),
                        tokens::truncate_tokens(&st.description, 30).replace('\n', " ")
                    );
                }
            }
            eprintln!(
                "  ${:.5} (planner ${:.5} in {} calls, worker ${:.5} in {} calls {:?}, decisions ${:.5} in {})",
                s.cost_usd, s.planner_cost_usd, s.planner_calls, s.worker_cost_usd, s.worker_calls, s.worker_by_purpose, s.decision_cost_usd, s.decision_calls
            );
            eprintln!("  answer: {}", tokens::truncate_tokens(&answer(&t), 80).replace('\n', " ⏎ "));
            let verdict = if t.status != TaskStatus::Completed { Err(format!("{:?}: {:?}", t.status, t.error)) } else { (c.check)(ws.path(), &t) };
            match verdict {
                Ok(()) => eprintln!("  PASS"),
                Err(why) => {
                    eprintln!("  FAIL: {why}");
                    failures.push(format!("{} (run {}): {why}", c.name, run_no + 1));
                }
            }
        }
    }
    free_ports();
    eprintln!("\n{} of {total} passed, ${cost:.4} in all", total - failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
