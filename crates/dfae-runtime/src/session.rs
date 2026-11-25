//! Conversation continuity at zero token cost: what the user was just talking about
//! (a file, a page, their city) is carried forward from the session's earlier turns,
//! and short references to it ("read it", "that file", "weather in my city") are
//! resolved by rules before routing, so every later stage sees a self-contained request.

use dfae_core::*;
use regex::Regex;
use std::sync::LazyLock;

/// The most recent file, URL and place mentioned in a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub path: Option<String>,
    pub url: Option<String>,
    pub location: Option<String>,
}

/// `tasks` oldest → newest; later turns win.
pub fn facts_from<'a>(tasks: impl IntoIterator<Item = &'a TaskRecord>) -> Facts {
    let mut f = Facts::default();
    for t in tasks {
        for e in &t.entities {
            let slot = match e.label.as_str() {
                "file path" | "path" => &mut f.path,
                "url" => &mut f.url,
                "location" => &mut f.location,
                _ => continue,
            };
            *slot = Some(e.text.clone());
        }
    }
    f
}

static MY_PLACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:my|our)\s+(?:current\s+)?(?:city|town|location|area|place|neighbou?rhood)\b|\bwhere\s+i\s+(?:live|am)\b").unwrap());
static HERE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:here|outside)\b").unwrap());
static THAT_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:that|this|the\s+same|the\s+above|the\s+previous|the\s+last)\s+file\b").unwrap());
static FILE_IT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?P<verb>read|open|show|cat|print|display|view|delete|remove|edit|update|summari[sz]e|check|list|append\s+to|write\s+to)\s+(?P<me>me\s+)?it\b",
    )
    .unwrap()
});
static THAT_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:that|this|the\s+same)\s+(?:url|link|page|site|website)\b").unwrap());

/// What the request could be leaning on, as the decision model is shown it (`file: a.txt`) and as the options it
/// chooses between. Only things that exist are offered.
pub fn known_things(facts: &Facts, own_place: Option<&str>, has_code: bool) -> (Vec<String>, Vec<(String, String)>) {
    let (mut known, mut options) = (Vec::new(), Vec::new());
    if let Some(p) = &facts.path {
        known.push(format!("file: {p}"));
        options.push(("file".to_string(), format!("the file named earlier ({p})")));
    }
    if let Some(u) = &facts.url {
        known.push(format!("url: {u}"));
        options.push(("url".to_string(), format!("the link given earlier ({u})")));
    }
    if let Some(pl) = own_place.map(String::from).or_else(|| facts.location.clone()) {
        known.push(format!("place: {pl}"));
        options.push(("place".to_string(), format!("the user's city or place ({pl})")));
    }
    if has_code {
        known.push("code: a code block from an earlier reply".to_string());
        options.push(("code".to_string(), "the code or page from an earlier reply".to_string()));
    }
    (known, options)
}

/// Make the request self-contained for the thing the decision model said it leans on. The model decides *whether*
/// and *what*; this only does the mechanical part: put the value where the reference stands (or after the request when
/// no phrase can be located), so rules, the planner and the binder all see it. `code` needs no rewriting: it is
/// available to every step by name (`{{prev_code}}`).
pub fn apply(goal: &str, entities: &[Entity], facts: &Facts, own_place: Option<&str>, kind: &str) -> (String, Vec<Entity>, Vec<String>) {
    let mut goal = goal.to_string();
    let mut ents = entities.to_vec();
    let mut notes = Vec::new();
    let add = |ents: &mut Vec<Entity>, label: &str, text: &str| {
        ents.push(Entity { label: label.into(), text: text.into(), start: None, end: None, score: 0.9, source: "session".into() });
    };
    match kind {
        "file" => {
            if let Some(path) = &facts.path {
                goal = if THAT_FILE.is_match(&goal) {
                    THAT_FILE.replace(&goal, path.as_str()).into_owned()
                } else if FILE_IT.is_match(&goal) {
                    FILE_IT.replace(&goal, format!("${{verb}} ${{me}}{path}").as_str()).into_owned()
                } else {
                    format!("{} ({path})", goal.trim_end())
                };
                add(&mut ents, "file path", path);
                notes.push(format!("it refers to the file '{path}'"));
            }
        }
        "url" => {
            if let Some(url) = &facts.url {
                goal = if THAT_URL.is_match(&goal) { THAT_URL.replace(&goal, url.as_str()).into_owned() } else { format!("{} ({url})", goal.trim_end()) };
                add(&mut ents, "url", url);
                notes.push(format!("it refers to the link '{url}'"));
            }
        }
        "place" => {
            if let Some(place) = own_place.map(String::from).or_else(|| facts.location.clone()) {
                goal = if MY_PLACE.is_match(&goal) {
                    MY_PLACE.replace(&goal, place.as_str()).into_owned()
                } else if HERE.is_match(&goal) {
                    HERE.replace(&goal, format!("in {place}").as_str()).into_owned()
                } else {
                    format!("{} (in {place})", goal.trim_end())
                };
                if !ents.iter().any(|e| e.label == "location") {
                    add(&mut ents, "location", &place);
                }
                notes.push(format!("your place is '{place}'"));
            }
        }
        _ => {}
    }
    (goal, ents, notes)
}

// -------------------------------------------------------------- artifacts ----

/// Names a step may use in its arguments for things said earlier in the conversation.
pub const ARTIFACT_NAMES: [&str; 2] = ["prev_code", "prev_reply"];

static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```([A-Za-z0-9_+.\-]*)[ \t]*\n(.*?)```").unwrap());

/// What the user can point at with "it", "that code", "the page above": the largest code block of the most recent reply
/// that had one (`prev_code`), and the most recent reply itself (`prev_reply`). `history` is oldest first.
/// Returns `(name, value, description)`.
pub fn artifacts_from(history: &[TaskRecord]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let replies: Vec<&str> = history.iter().rev().filter_map(|t| t.final_answer.as_deref()).filter(|a| !a.trim().is_empty()).collect();
    if let Some(last) = replies.first() {
        out.push(("prev_reply".to_string(), last.to_string(), format!("the last reply, {} chars", last.chars().count())));
    }
    for reply in replies.iter().take(3) {
        let best = FENCE.captures_iter(reply).map(|c| (c[1].trim().to_string(), c[2].to_string())).max_by_key(|(_, code)| code.len());
        if let Some((lang, code)) = best.filter(|(_, code)| !code.trim().is_empty()) {
            let what = if lang.is_empty() { "code".to_string() } else { format!("{lang} code") };
            out.push(("prev_code".to_string(), code.clone(), format!("{what} block from an earlier reply, {} lines", code.lines().count())));
            break;
        }
    }
    out
}

/// Replace `{{prev_code}}`-style references in step arguments with the values themselves.
pub fn substitute_artifacts(args: &serde_json::Value, artifacts: &[(String, String)]) -> serde_json::Value {
    use serde_json::Value;
    match args {
        Value::String(s) => {
            let mut out = s.clone();
            for (name, value) in artifacts {
                out = out.replace(&format!("{{{{{name}}}}}"), value);
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute_artifacts(x, artifacts)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), substitute_artifacts(x, artifacts))).collect()),
        other => other.clone(),
    }
}

// ------------------------------------------------------------- small talk ----

/// The reply to a purely social message, by the kind the decision model recognised. No model call.
/// `can` is a one-line list of what the agent can do (built from its tools).
pub fn small_talk_reply(kind: &str, can: &str) -> Option<String> {
    Some(match kind {
        "greeting" => format!("Hi! I'm DFAE: I chat and use tools ({can}). What do you need?"),
        "how_are_you" => "Doing well, thanks. What do you need?".into(),
        "thanks" => "You're welcome.".into(),
        "farewell" => "Goodbye.".into(),
        "about_you" => format!("I'm DFAE: I chat and use tools ({can}). What do you need?"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(l: &str, t: &str) -> Entity {
        Entity { label: l.into(), text: t.into(), start: None, end: None, score: 1.0, source: "t".into() }
    }

    fn task(entities: Vec<Entity>) -> TaskRecord {
        let mut t = TaskRecord::new(TaskRequest { goal: "g".into(), ..Default::default() }, Budget::default());
        t.entities = entities;
        t
    }

    #[test]
    fn newest_mention_wins() {
        let (a, b) = (task(vec![ent("file path", "a.txt"), ent("location", "Paris")]), task(vec![ent("file path", "b.txt")]));
        assert_eq!(facts_from([&a, &b]), Facts { path: Some("b.txt".into()), url: None, location: Some("Paris".into()) });
    }

    #[test]
    fn only_things_that_exist_are_offered_to_the_decision_model() {
        let f = Facts { path: Some("test.txt".into()), url: None, location: None };
        let (known, options) = known_things(&f, None, false);
        assert_eq!((known, options.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()), (vec!["file: test.txt".to_string()], vec!["file"]));
        let (known, options) = known_things(&Facts::default(), Some("Berlin"), true);
        assert_eq!(known, ["place: Berlin", "code: a code block from an earlier reply"]);
        assert_eq!(options.len(), 2);
        assert!(known_things(&Facts::default(), None, false).1.is_empty(), "nothing to refer to: the model is not even asked");
    }

    #[test]
    fn the_chosen_reference_is_made_explicit() {
        let f = Facts { path: Some("test.txt".into()), url: Some("https://example.com/x".into()), location: Some("new york".into()) };
        let go = |g: &str, kind: &str| apply(g, &[], &f, None, kind).0;
        assert_eq!(go("read it", "file"), "read test.txt");
        assert_eq!(go("show me it again", "file"), "show me test.txt again");
        assert_eq!(go("delete that file", "file"), "delete test.txt");
        assert_eq!(go("what's the weather in my city?", "place"), "what's the weather in new york?");
        assert_eq!(go("is it raining here", "place"), "is it raining in new york");
        assert_eq!(go("fetch that link", "url"), "fetch https://example.com/x");
        // no phrase to replace: the value is added after the request, where rules and models can still see it
        assert_eq!(go("turn the lights off, same as before", "file"), "turn the lights off, same as before (test.txt)");
        assert_eq!(go("anything", "none"), "anything");
        assert_eq!(go("anything", "code"), "anything", "code is available by name, no rewriting");
        let (g, e, n) = apply("read it", &[], &f, None, "file");
        assert!(e.iter().any(|x| x.label == "file path" && x.text == "test.txt") && n.len() == 1 && g == "read test.txt");
        // a place said in this very message beats an older one
        assert_eq!(apply("weather in my city", &[], &f, Some("berlin"), "place").0, "weather in berlin");
        // a kind that has no value does nothing
        assert_eq!(apply("read it", &[], &Facts::default(), None, "file").0, "read it");
    }

    #[test]
    fn small_talk_replies_are_short_and_by_kind() {
        let can = "files, shell";
        assert!(small_talk_reply("greeting", can).unwrap().starts_with("Hi! I'm DFAE"));
        assert!(small_talk_reply("how_are_you", can).unwrap().contains("oing well"));
        assert!(small_talk_reply("thanks", can).unwrap().contains("welcome"));
        assert!(small_talk_reply("about_you", can).unwrap().contains("files, shell"));
        assert!(small_talk_reply("none", can).is_none());
    }

    #[test]
    fn what_the_user_can_point_at_is_the_code_of_the_last_reply() {
        let mut t = task(vec![]);
        t.final_answer = Some("Here you go:\n```html\n<h1>Hi</h1>\n<p>x</p>\n```\nAnd some css:\n```css\nh1{}\n```".into());
        let mut chat = task(vec![]);
        chat.final_answer = Some("Sure, anything else?".into());
        let a = artifacts_from(&[t.clone(), chat]);
        let get = |n: &str| a.iter().find(|(k, _, _)| k == n).cloned().unwrap();
        assert_eq!(get("prev_reply").1, "Sure, anything else?");
        let (_, code, what) = get("prev_code");
        assert_eq!(code, "<h1>Hi</h1>\n<p>x</p>\n", "the largest block of the latest reply that had code");
        assert!(what.contains("html code") && what.contains("2 lines"), "{what}");
        assert!(artifacts_from(&[task(vec![])]).is_empty());
        let args = serde_json::json!({"path": "index.html", "content": "{{prev_code}}", "n": 1});
        let out = substitute_artifacts(&args, &[("prev_code".into(), "<b>x</b>".into())]);
        assert_eq!(out, serde_json::json!({"path": "index.html", "content": "<b>x</b>", "n": 1}));
    }
}
