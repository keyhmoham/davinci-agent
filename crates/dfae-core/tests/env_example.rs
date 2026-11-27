//! `.env.example` shows the defaults: loading every value it sets gives the same configuration as setting nothing.

#[test]
fn the_example_env_file_holds_the_defaults() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env.example")).unwrap();
    let pairs: Vec<(String, String)> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.split(" #").next().unwrap_or("").trim().to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect();
    assert!(pairs.len() > 30, "the file was read: {}", pairs.len());
    let snapshot = || {
        let mut v = serde_json::to_value(dfae_core::Config::from_env()).unwrap();
        v["workspace"] = serde_json::Value::Null; // the working directory, not a setting
        v
    };
    // SAFETY: this test binary has one test, so nothing else reads the environment meanwhile
    unsafe {
        for (k, _) in &pairs {
            std::env::remove_var(k);
        }
        std::env::set_var("PLANNER_API_KEY", "key"); // the hosted defaults need a key
    }
    let defaults = snapshot();
    unsafe {
        for (k, v) in &pairs {
            std::env::set_var(k, v);
        }
    }
    let from_file = snapshot();
    let (d, f) = (defaults.as_object().unwrap(), from_file.as_object().unwrap());
    let differ: Vec<String> = d
        .keys()
        .flat_map(|k| match (&d[k], &f[k]) {
            (serde_json::Value::Object(a), serde_json::Value::Object(b)) => a
                .keys()
                .filter(|s| a.get(*s) != b.get(*s))
                .map(|s| format!("{k}.{s}: default {} / file {}", a[s], b.get(s).unwrap_or(&serde_json::Value::Null)))
                .collect::<Vec<_>>(),
            (a, b) if a != b => vec![format!("{k}: default {a} / file {b}")],
            _ => vec![],
        })
        .collect();
    assert!(differ.is_empty(), "values in .env.example that are not the defaults:\n{}", differ.join("\n"));
}
