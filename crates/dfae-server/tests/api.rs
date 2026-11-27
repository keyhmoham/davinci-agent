#![allow(clippy::field_reassign_with_default)]
use dfae_core::Config;
use serde_json::{Value, json};
use std::time::Duration;

async fn spawn(token: Option<&str>) -> (String, tempfile::TempDir) {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    let engine = dfae_runtime::build_engine(cfg).await.unwrap();
    let app = dfae_server::router(engine, token.map(String::from));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), ws)
}

async fn wait_done(c: &reqwest::Client, base: &str, id: &str) -> Value {
    for _ in 0..300 {
        let v: Value = c.get(format!("{base}/v1/tasks/{id}")).send().await.unwrap().json().await.unwrap();
        if matches!(v["status"].as_str(), Some("completed" | "failed" | "cancelled")) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("task did not finish");
}

#[tokio::test]
async fn task_lifecycle_over_http() {
    let (base, _ws) = spawn(None).await;
    let c = reqwest::Client::new();
    assert_eq!(c.get(format!("{base}/health")).send().await.unwrap().status(), 200);
    let r = c.post(format!("{base}/v1/tasks")).json(&json!({"goal": "what is 6 * 7?"})).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let id = r.json::<Value>().await.unwrap()["task_id"].as_str().unwrap().to_string();
    let t = wait_done(&c, &base, &id).await;
    assert_eq!(t["status"], "completed");
    assert!(t["final_answer"].as_str().unwrap().contains("42"));
    assert_eq!(t["mode"], "direct");
    let list: Vec<Value> = c.get(format!("{base}/v1/tasks")).send().await.unwrap().json().await.unwrap();
    assert_eq!(list[0]["task_id"], id);
    assert!(list[0]["ledger"]["baseline_tokens"].as_u64().unwrap() > 0);
    let audit: Value = c.get(format!("{base}/v1/tasks/{id}/audit")).send().await.unwrap().json().await.unwrap();
    assert_eq!(audit["chain_valid"], true);
    let stats: Value = c.get(format!("{base}/v1/stats")).send().await.unwrap().json().await.unwrap();
    assert_eq!(stats["tasks"], 1);
    let m = c.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("dfae_tasks 1"));
    let info: Value = c.get(format!("{base}/v1/info")).send().await.unwrap().json().await.unwrap();
    assert!(info["tools"].as_array().unwrap().len() >= 10);
    assert!(info["config"].get("api_key").is_none());
}

#[tokio::test]
async fn sse_replays_backlog_and_ends_with_final() {
    let (base, _ws) = spawn(None).await;
    let c = reqwest::Client::new();
    let id = c.post(format!("{base}/v1/tasks")).json(&json!({"goal": "what time is it?"})).send().await.unwrap().json::<Value>().await.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_done(&c, &base, &id).await;
    // attach after completion: the whole history is replayed
    let mut resp = c.get(format!("{base}/v1/tasks/{id}/events")).send().await.unwrap();
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !text.contains("event: final") && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(Some(chunk))) = tokio::time::timeout(Duration::from_millis(500), resp.chunk()).await {
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    for ev in ["task_created", "extraction", "decision", "mode", "plan_created", "step_update", "verification", "final"] {
        assert!(text.contains(&format!("event: {ev}")), "missing {ev} in:\n{text}");
    }
}

#[tokio::test]
async fn bearer_token_is_enforced() {
    let (base, _ws) = spawn(Some("s3cret")).await;
    let c = reqwest::Client::new();
    assert_eq!(c.get(format!("{base}/health")).send().await.unwrap().status(), 200, "liveness stays open");
    assert_eq!(c.get(format!("{base}/v1/tasks")).send().await.unwrap().status(), 401);
    assert_eq!(c.get(format!("{base}/v1/tasks")).bearer_auth("wrong").send().await.unwrap().status(), 401);
    assert_eq!(c.get(format!("{base}/v1/tasks")).bearer_auth("s3cret").send().await.unwrap().status(), 200);
}

#[tokio::test]
async fn validation_errors() {
    let (base, _ws) = spawn(None).await;
    let c = reqwest::Client::new();
    assert_eq!(c.post(format!("{base}/v1/tasks")).json(&json!({"goal": "   "})).send().await.unwrap().status(), 400);
    assert_eq!(c.post(format!("{base}/v1/tasks")).json(&json!({"goal": "x", "auto_approve": true})).send().await.unwrap().status(), 400);
    assert_eq!(c.get(format!("{base}/v1/tasks/nope")).send().await.unwrap().status(), 404);
    assert_eq!(c.post(format!("{base}/v1/tasks/nope/cancel")).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn memory_api() {
    let (base, _ws) = spawn(None).await;
    let c = reqwest::Client::new();
    c.post(format!("{base}/v1/memory")).json(&json!({"text": "the deploy key rotates every 90 days"})).send().await.unwrap();
    let r: Vec<Value> = c.get(format!("{base}/v1/memory/search?q=when+does+the+deploy+key+rotate")).send().await.unwrap().json().await.unwrap();
    assert!(r[0]["item"]["text"].as_str().unwrap().contains("90 days"));
    assert_eq!(c.post(format!("{base}/v1/memory")).json(&json!({"text": "x", "kind": "procedural"})).send().await.unwrap().status(), 400);
}

#[tokio::test]
async fn dispatch_and_team_fields_are_exposed() {
    let (base, _ws) = spawn(None).await;
    let c = reqwest::Client::new();
    let d: Value = c.get(format!("{base}/v1/dispatch")).send().await.unwrap().json().await.unwrap();
    assert!(d["tasks"]["capacity"].as_u64().unwrap() >= 1);
    assert_eq!(d["roles"]["operator"]["capacity"], 1, "side-effecting agents are serialised by default");
    assert!(d["roles"]["researcher"]["capacity"].as_u64().unwrap() > 1);
    let id = c.post(format!("{base}/v1/tasks")).json(&json!({"goal": "what is 2 + 2"})).send().await.unwrap().json::<Value>().await.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_done(&c, &base, &id).await;
    let list: Vec<Value> = c.get(format!("{base}/v1/tasks")).send().await.unwrap().json().await.unwrap();
    assert!(list[0]["role"].is_null() && list[0]["parent_id"].is_null() && list[0]["agents"] == 0, "a plain task is not a team");
    let m = c.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("dfae_teams 0") && m.contains("dfae_dispatch_agents{state=\"running\"} 0"), "{m}");
    let info: Value = c.get(format!("{base}/v1/info")).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["config"]["team"]["enabled"], true);
    assert_eq!(info["config"]["economy"]["repair"], true);
    assert_eq!(info["config"]["economy"]["lazy_context"], false, "lazy context is opt-in");
    let cost: Value = c.get(format!("{base}/v1/cost")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cost["plan"]["prior"], 330.0);
    assert!(cost["step"]["estimate"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn routines_and_the_board_over_http() {
    let (base, _ws) = spawn(None).await;
    let c = reqwest::Client::new();
    // a person adds a routine: enabled, scheduled
    let r = c.post(format!("{base}/v1/routines")).json(&json!({"goal": "what is 6 * 7?", "when": "every 2h", "name": "sum"})).send().await.unwrap();
    assert_eq!(r.status(), 201);
    let rt: Value = r.json().await.unwrap();
    let id = rt["id"].as_str().unwrap().to_string();
    assert_eq!(rt["enabled"], true);
    assert_eq!(rt["origin"]["by"], "user");
    assert!(rt["state"]["next_run"].is_string());
    // a bad schedule is refused with the forms that work
    let bad = c.post(format!("{base}/v1/routines")).json(&json!({"goal": "x", "when": "whenever"})).send().await.unwrap();
    assert_eq!(bad.status(), 400);
    assert!(bad.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("every 2h"));
    // run it now: a task with the routine on it, shown on the board
    let run: Value = c.post(format!("{base}/v1/routines/{id}/run")).send().await.unwrap().json().await.unwrap();
    let task = run["task_id"].as_str().unwrap().to_string();
    let t = wait_done(&c, &base, &task).await;
    assert_eq!(t["request"]["routine"], id.as_str());
    let mut board = Value::Null;
    for _ in 0..100 {
        board = c.get(format!("{base}/v1/board")).send().await.unwrap().json().await.unwrap();
        if board["routines"][0]["history"].as_array().is_some_and(|h| !h.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(board["routines"][0]["id"], id.as_str());
    assert_eq!(board["routines"][0]["history"][0]["outcome"], "ok", "{board}");
    assert!(board["recent"].as_array().unwrap().iter().any(|x| x["task_id"] == task.as_str() && x["routine_id"] == id.as_str()));
    // pause, change the schedule, remove
    let p: Value = c.patch(format!("{base}/v1/routines/{id}")).json(&json!({"enabled": false})).send().await.unwrap().json().await.unwrap();
    assert_eq!(p["enabled"], false);
    assert!(p["state"]["next_run"].is_null());
    let w: Value =
        c.patch(format!("{base}/v1/routines/{id}")).json(&json!({"when": "weekdays 08:30", "enabled": true})).send().await.unwrap().json().await.unwrap();
    assert_eq!(w["schedule"]["expr"], "30 8 * * 1-5");
    assert_eq!(c.delete(format!("{base}/v1/routines/{id}")).send().await.unwrap().status(), 200);
    assert_eq!(c.get(format!("{base}/v1/routines/{id}")).send().await.unwrap().status(), 404);
    // a caller cannot claim a task was started by a routine
    let forged = c.post(format!("{base}/v1/tasks")).json(&json!({"goal": "what is 1 + 1?", "routine": "nope"})).send().await.unwrap();
    let fid = forged.json::<Value>().await.unwrap()["task_id"].as_str().unwrap().to_string();
    assert!(wait_done(&c, &base, &fid).await["request"]["routine"].is_null());
}
