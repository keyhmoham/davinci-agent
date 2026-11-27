//! Recipe cards: the official generator of a framework, pinned to a major the worker knows well (coding plan B2).
//!
//! Every Next.js plan used to hand-write `package.json` with `"next": "latest"`: no tsconfig, no config, no lint, and whatever major npm
//! resolved that day (the worker then invented flags for it). A card is chosen by code from the request's words and goes in the planner's
//! USER message (its cached system prompt stays byte-identical); a request that matches no card sees nothing.
//!
//! The scaffold flags are the generators' documented ones (create-next-app's CLI reference; Vite's guide). A card is bumped deliberately,
//! with a live run, never by whatever a registry resolves today.

use regex::Regex;
use std::sync::LazyLock;

pub(crate) struct Recipe {
    pub name: &'static str,
    trigger: &'static str,
    /// `{dir}` is the project directory, `{port}` the port it is served on.
    pub card: &'static str,
}

static RECIPES: LazyLock<Vec<(Regex, &'static Recipe)>> = LazyLock::new(|| {
    static ALL: &[Recipe] = &[
        Recipe {
            name: "nextjs",
            trigger: r"(?i)\bnext\.?js\b|\bnext\s+(?:app|site|project)\b",
            card: "RECIPE nextjs (create-next-app 15, pinned): scaffold with ONE shell step `npx -y create-next-app@15 {dir} --ts --tailwind --eslint --app --src-dir --use-npm --import-alias \"@/*\" --yes` (it installs the dependencies; allow it 600 s); then change {dir}/src/app/page.tsx (and {dir}/src/app/globals.css or layout.tsx if needed) with fs_write/fs_edit; build with `npm run build` (cwd {dir}); serve with `npm run start -- -H 0.0.0.0 -p {port}` in the background (cwd {dir}). Never write package.json, tsconfig or next.config by hand.",
        },
        Recipe {
            name: "vite-react",
            trigger: r"(?i)\bvite\b.*\breact\b|\breact\b.*\bvite\b|\breact\s+(?:app|site|project|spa)\b",
            card: "RECIPE vite-react (create-vite 6, pinned): scaffold with `npx -y create-vite@6 {dir} --template react-ts`, then `npm install` (cwd {dir}); change {dir}/src/App.tsx and {dir}/src/App.css / index.css; build with `npm run build`; serve the build with `npx -y vite@6 preview --host 0.0.0.0 --port {port}` in the background (cwd {dir}). Never write package.json by hand.",
        },
        Recipe {
            name: "vite-vue",
            trigger: r"(?i)\bvue(?:\.?js)?\b",
            card: "RECIPE vite-vue (create-vite 6, pinned): scaffold with `npx -y create-vite@6 {dir} --template vue-ts`, then `npm install` (cwd {dir}); change {dir}/src/App.vue and {dir}/src/components/; build with `npm run build`; serve with `npx -y vite@6 preview --host 0.0.0.0 --port {port}` in the background (cwd {dir}). Never write package.json by hand.",
        },
        Recipe {
            name: "express",
            trigger: r"(?i)\bexpress(?:\.?js)?\b",
            card: "RECIPE express (Express 4, pinned): `npm init -y` then `npm install express@4` (cwd {dir}); write {dir}/server.js (CommonJS, `app.listen(process.env.PORT || {port}, '0.0.0.0')`); serve with `node server.js` in the background (cwd {dir}). Pin every dependency's major (`x@4`), never `latest`.",
        },
        Recipe {
            name: "fastapi",
            trigger: r"(?i)\bfast\s?api\b",
            card: "RECIPE fastapi: `python3 -m venv .venv && .venv/bin/pip install 'fastapi>=0.115,<1' 'uvicorn[standard]>=0.30,<1'` (cwd {dir}); write {dir}/main.py with `app = FastAPI()`; serve with `.venv/bin/uvicorn main:app --host 0.0.0.0 --port {port}` in the background (cwd {dir}); pin versions in {dir}/requirements.txt.",
        },
        Recipe {
            name: "flask",
            trigger: r"(?i)\bflask\b",
            card: "RECIPE flask (Flask 3, pinned): `python3 -m venv .venv && .venv/bin/pip install 'flask>=3,<4'` (cwd {dir}); write {dir}/app.py; serve with `.venv/bin/flask --app app run --host 0.0.0.0 --port {port}` in the background (cwd {dir}); pin versions in {dir}/requirements.txt.",
        },
        Recipe {
            name: "django",
            trigger: r"(?i)\bdjango\b",
            card: "RECIPE django (Django 5.1, pinned): `mkdir -p {dir} && python3 -m venv {dir}/.venv && {dir}/.venv/bin/pip install 'django>=5.1,<5.2'`; scaffold with `.venv/bin/django-admin startproject site_core .` (cwd {dir}) — the generator writes manage.py and settings; add ALLOWED_HOSTS = ['*'] with fs_edit; serve with `.venv/bin/python manage.py runserver 0.0.0.0:{port}` in the background (cwd {dir}).",
        },
        Recipe {
            name: "rust-cli",
            trigger: r"(?i)\brust\b.*\b(?:cli|command[- ]line|tool|program|binary)\b|\bcargo\s+new\b",
            card: "RECIPE rust-cli: scaffold with `cargo new {dir}` (never write Cargo.toml by hand; add crates with `cargo add <crate>@<major>` in cwd {dir}); change {dir}/src/main.rs; verify with `cargo build` and `cargo run -- <args>` (cwd {dir}).",
        },
        Recipe {
            name: "python-cli",
            trigger: r"(?i)\bpython\b.*\b(?:cli|command[- ]line|tool|script)\b",
            card: "RECIPE python-cli: write {dir}/<name>.py with argparse and a `main()`, and {dir}/requirements.txt with pinned versions if it needs packages (`pkg>=X,<X+1`), installed into `{dir}/.venv`; verify with `python3 {dir}/<name>.py --help` and one real run.",
        },
    ];
    ALL.iter().map(|r| (Regex::new(r.trigger).expect("recipe trigger"), r)).collect()
});

/// The recipe a request asks for (the first that matches), if any.
pub(crate) fn matching(request: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|(re, _)| re.is_match(request)).map(|(_, r)| *r)
}

/// The card for the planner's user message: the recipe, with a directory named for the request and a free port.
pub(crate) fn card_for(request: &str, dir: &str, port: u16) -> Option<String> {
    let r = matching(request)?;
    Some(format!("\n{}", r.card.replace("{dir}", dir).replace("{port}", &port.to_string())))
}

/// Dependency specs that pin nothing (`latest`, `*`, `next`, an empty string) in a `package.json` a model wrote: what npm resolves for
/// them changes from day to day (Next 16 arrived unannounced under `latest`).
pub(crate) fn unpinned_deps(package_json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(package_json) else { return vec![] };
    let mut out = Vec::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(m) = v.get(key).and_then(|d| d.as_object()) {
            for (name, spec) in m {
                let s = spec.as_str().unwrap_or("").trim();
                if matches!(s, "" | "*" | "latest" | "next" | "x") {
                    out.push(format!("{name}: \"{s}\""));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_framework_request_gets_its_pinned_generator_and_others_get_nothing() {
        let c = card_for("create a professional landing page about a scientific laboratory using nextjs and build and serve it", "lab-landing", 8001).unwrap();
        assert!(c.contains("create-next-app@15 lab-landing") && c.contains("-p 8001") && c.contains("--yes"), "{c}");
        assert!(!c.contains("--no-import-alias"), "not a documented flag");
        assert_eq!(matching("a landing page with Next.js").map(|r| r.name), Some("nextjs"));
        assert_eq!(matching("a React app built with Vite").map(|r| r.name), Some("vite-react"));
        assert_eq!(matching("an API with FastAPI and sqlite").map(|r| r.name), Some("fastapi"));
        assert_eq!(matching("a Rust CLI that counts words").map(|r| r.name), Some("rust-cli"));
        assert!(matching("write a poem about the sea into poem.txt").is_none());
        assert!(matching("what is the next step").is_none(), "the word next is not Next.js");
    }

    #[test]
    fn unpinned_dependencies_are_named() {
        let bad = r#"{"dependencies": {"next": "latest", "react": "^19.1.0", "x": "*"}}"#;
        assert_eq!(unpinned_deps(bad), ["next: \"latest\"", "x: \"*\""]);
        assert!(unpinned_deps(r#"{"dependencies": {"next": "15.5.4"}}"#).is_empty());
    }
}
