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
static WEATHERISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:weather|temperature|forecast|rain\w*|snow\w*|hot|cold|warm)\b").unwrap());
static THAT_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:that|this|the\s+same|the\s+above|the\s+previous|the\s+last)\s+file\b").unwrap());
static FILE_IT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?P<verb>read|open|show|cat|print|display|view|delete|remove|edit|update|summari[sz]e|check|list|append\s+to|write\s+to)\s+(?P<me>me\s+)?it\b",
    )
    .unwrap()
});
static THAT_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:that|this|the\s+same)\s+(?:url|link|page|site|website)\b").unwrap());

fn has(entities: &[Entity], labels: &[&str]) -> bool {
    entities.iter().any(|e| labels.contains(&e.label.as_str()))
}

/// Rewrite references to earlier turns using `facts`. Returns the new request, the entities
/// it now contains, and what was resolved (for the event log). Unchanged if nothing applies.
pub fn resolve(goal: &str, entities: &[Entity], facts: &Facts) -> (String, Vec<Entity>, Vec<String>) {
    let mut goal = goal.to_string();
    let mut ents = entities.to_vec();
    let mut notes = Vec::new();
    let add = |ents: &mut Vec<Entity>, label: &str, text: &str| {
        ents.push(Entity { label: label.into(), text: text.into(), start: None, end: None, score: 0.9, source: "session".into() });
    };

    // a place: said in this very message ("I'm from Paris. weather in my city") or earlier in the session
    let place = entities.iter().find(|e| e.label == "location").map(|e| e.text.clone()).or_else(|| facts.location.clone());
    if let Some(place) = &place {
        let weatherish = WEATHERISH.is_match(&goal);
        if MY_PLACE.is_match(&goal) || (weatherish && HERE.is_match(&goal)) {
            goal = if MY_PLACE.is_match(&goal) { MY_PLACE.replace(&goal, place.as_str()) } else { HERE.replace(&goal, format!("in {place}").as_str()) }
                .into_owned();
            if !has(&ents, &["location"]) {
                add(&mut ents, "location", place);
            }
            notes.push(format!("your place is '{place}'"));
        }
    }
    if let Some(path) = &facts.path
        && !has(&ents, &["file path", "path", "url"])
    {
        if THAT_FILE.is_match(&goal) {
            goal = THAT_FILE.replace(&goal, path.as_str()).into_owned();
            add(&mut ents, "file path", path);
            notes.push(format!("'that file' is '{path}'"));
        } else if FILE_IT.is_match(&goal) {
            goal = FILE_IT.replace(&goal, format!("${{verb}} ${{me}}{path}").as_str()).into_owned();
            add(&mut ents, "file path", path);
            notes.push(format!("'it' is '{path}'"));
        }
    }
    if let Some(url) = &facts.url
        && !has(&ents, &["url"])
        && THAT_URL.is_match(&goal)
    {
        goal = THAT_URL.replace(&goal, url.as_str()).into_owned();
        add(&mut ents, "url", url);
        notes.push(format!("'that link' is '{url}'"));
    }
    (goal, ents, notes)
}

// ------------------------------------------------------------- small talk ----

static GREETING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:hi|hello|hey|hiya|howdy|yo|greetings|good\s+(?:morning|afternoon|evening|day)|sup)\b[\s,.!-]*").unwrap());
static HOW_ARE_YOU: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:so[,\s]+|and\s+)?(?:tell\s+me\s+|can\s+you\s+tell\s+me\s+)?(?:how\s+are\s+(?:you|u)(?:\s+(?:doing|today|going))*|how\s+do\s+you\s+do|how(?:'s|\s+is)\s+it\s+going|how\s+have\s+you\s+been|what'?s\s+up|how(?:'re|\s+are)\s+things)\s*[?!.]*$").unwrap()
});
static THANKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:thanks|thank\s+you|thx|ty|cheers|much\s+appreciated|great,?\s+thanks)(?:\s+(?:a\s+lot|so\s+much|very\s+much|again))?\s*[.!]*$")
        .unwrap()
});
static BYE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:bye|goodbye|see\s+you|see\s+ya|good\s*night|cya)(?:\s+\w+)?\s*[.!]*$").unwrap());
static WHO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:who\s+are\s+you|what\s+are\s+you|what\s+can\s+you\s+do|what\s+do\s+you\s+do|help|what\s+are\s+your\s+(?:capabilities|abilities))\s*[?!.]*$",
    )
    .unwrap()
});

/// Replies that need no model: greetings, thanks, "how are you", "what can you do".
/// `can` is a one-line list of what the agent can do (built from its tools).
pub fn small_talk(goal: &str, can: &str) -> Option<String> {
    let greeted = GREETING.find(goal);
    let rest = greeted.map(|m| &goal[m.end()..]).unwrap_or(goal).trim();
    let rest = rest.trim_matches(|c: char| c == '.' || c == ',' || c.is_whitespace());
    let intro = format!("I'm DFAE: I chat and use tools ({can}). What do you need?");
    if rest.is_empty() {
        return greeted.map(|_| format!("Hi! {intro}"));
    }
    if HOW_ARE_YOU.is_match(rest) {
        return Some("Doing well, thanks. What do you need?".into());
    }
    if THANKS.is_match(rest) {
        return Some("You're welcome.".into());
    }
    if BYE.is_match(rest) {
        return Some("Goodbye.".into());
    }
    if WHO.is_match(rest) {
        return Some(intro);
    }
    None
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
    fn resolves_references_to_the_previous_turns() {
        let f = Facts { path: Some("test.txt".into()), url: Some("https://example.com/x".into()), location: Some("new york".into()) };
        let (g, e, n) = resolve("read it", &[], &f);
        assert_eq!(g, "read test.txt");
        assert!(e.iter().any(|x| x.label == "file path" && x.text == "test.txt") && n.len() == 1);
        assert_eq!(resolve("show me it again", &[], &f).0, "show me test.txt again");
        assert_eq!(resolve("delete that file", &[], &f).0, "delete test.txt");
        assert_eq!(resolve("what's the weather in my city?", &[], &f).0, "what's the weather in new york?");
        assert_eq!(resolve("is it raining here", &[], &f).0, "is it raining in new york");
        assert_eq!(resolve("fetch that link", &[], &f).0, "fetch https://example.com/x");
        // an explicit file wins; "it" without a file verb is left alone; "here" without weather is left alone
        assert_eq!(resolve("read notes.md", &[ent("file path", "notes.md")], &f).0, "read notes.md");
        assert_eq!(resolve("it is fine", &[], &f).0, "it is fine");
        assert_eq!(resolve("list files here", &[], &f).0, "list files here");
        // nothing known: nothing invented
        assert_eq!(resolve("read it", &[], &Facts::default()).0, "read it");
        // a place given in the same message beats an older one
        let (g, _, _) = resolve("i'm from berlin. weather in my city", &[ent("location", "berlin")], &f);
        assert_eq!(g, "i'm from berlin. weather in berlin");
    }

    #[test]
    fn small_talk_needs_no_model() {
        let can = "files, shell";
        assert!(small_talk("hi", can).unwrap().starts_with("Hi! I'm DFAE"));
        assert!(small_talk("Hello!!", can).is_some());
        assert!(small_talk("hi. tell me how are you ?", can).unwrap().contains("oing well"));
        assert!(small_talk("how are you?", can).is_some());
        assert!(small_talk("thanks a lot!", can).unwrap().contains("welcome"));
        assert!(small_talk("bye", can).unwrap().contains("Goodbye"));
        assert!(small_talk("what can you do?", can).unwrap().contains("files, shell"));
        // anything with a real question or task goes on to the pipeline
        for s in [
            "hi. create a file named a.txt",
            "hello, what is the capital of France?",
            "hi. i'm keyhan from new york. tell me weather",
            "how are you going to fix the build",
        ] {
            assert!(small_talk(s, can).is_none(), "{s}");
        }
    }
}
