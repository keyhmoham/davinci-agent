# Coding & shell upgrade plan — why "a Next.js landing page" fails, and what fixes it

> Status: proposal, 2026-11-27. Nothing here is implemented yet.
> Same rules as [`upgrade-plan.md`](upgrade-plan.md): each item has **Problem → Evidence → Fix → Files → Done when**, ships behind a flag
> where behaviour changes, and is kept only if the live suite says so. Evidence labels: `[seen]` = in a real task record or reproduced in
> the running container today; `[code]` = read in the code; `[hypothesis]` = best explanation, not yet proven.

---

## 0. What was examined

The four real failures of the request "create a professional landing page … using nextjs and build and serve it" stored in Postgres
(`8d665de7`, `7643b309`, `15000756`, `6e0c3ebc`), the running container (`dfae-dfae-1`), and the code paths they went through:
`implement.rs` (binding, content writing), `executor.rs` (tool run, recovery, step agent), `step_agent.rs`, `syntax.rs`,
`dfae-tools/src/{coding,native,toolbox}.rs`, `dfae-extract/src/rules.rs`, `prompts.rs`.

**Result: 0 of 4 real attempts succeeded, while the live suite's Next.js case passes 14/14.** The failures are not one bug. Each task hit
3–5 independent defects, and every defect fed the next one: a killed install → a broken `node_modules` → a crash the worker cannot
diagnose → six identical fix turns → budget gone → a port clash nobody could fix any more.

| Task | Tokens (in/out) | $ | Ended by |
|---|---:|---:|---|
| `8d665de7` crypto | 24.5k / 19.7k | — | `npm: command not found` (old image) after a hand-written `package.json` |
| `7643b309` crypto | 33.5k / 24.5k | — | `npm install && npm run build` **killed at 120 s** |
| `15000756` science | 24.5k / 21.7k | — | `ld.so` crash in `next build`, then a gateway transport error during re-plan |
| `6e0c3ebc` lab | 71.4k / 8.1k | $0.0129 | 12 fix turns (33k tokens), tool budget 25 exhausted, port 8000 taken, time budget exceeded |
| `b8461ee9` plain-HTML earth page (for comparison, **completed**) | 2.5k / 3.1k | — | — |

---

## 1. Root causes, ranked by how much they cost

### Execution layer (deterministic code, fixes cost 0 tokens)

| ID | Root cause | Evidence |
|---|---|---|
| **X1** | **The executor kills every tool call after a hard 120 s** (`executor.rs:680`, `tokio::time::timeout(Duration::from_secs(120), tool.execute(..))`), overriding the shell's own 600 s install/build timeout and the `timeout_secs` the model asks for. No output is kept: the step only says "tool timed out after 120s". | `[code]`; `[seen]` `7643b309`: "`npm install && npm run build` failed with timeout: tool timed out after 12…" |
| **X2** | **A killed command leaves its children running.** When X1 (or a cancel) drops the `run_script` future, `kill_on_drop` SIGKILLs only `bash`; `npm` and `node` live on in the process group and keep writing into `node_modules`. | `[code]` `coding.rs` `run_script`: `killpg` runs only on the inner timeout/cancel branches, never on drop. `[seen]` `6e0c3ebc` r1_1_fix1: "Stale node_modules caused rmdir ENOTEMPTY" |
| **X3** | **The `ld.so` crash comes from a half-written native module, not from Next.js.** Every `next build` in `/workspace/site` died with `Inconsistency detected by ld.so … elf_machine_rela_relative`, under Turbopack and webpack alike. | `[seen]` A clean install of the same `next@16.4.6` in the same container (in `/tmp`) loads `next-swc.linux-x64-gnu.node` and runs the build. glibc 2.36, the toolbox's `LD_LIBRARY_PATH` holds nothing. `[hypothesis]` The binary in the workspace was truncated by X1+X2. The broken `node_modules` was later deleted by the agent, so this can no longer be proven directly. |
| **X4** | **`strip_fences` cuts every file at its first `<html`.** Its "a sentence before a bare HTML document" branch applies to *any* path, so a `layout.jsx` lost `export default function RootLayout({ children }) { return (`. | `[code]` `implement.rs:63-69`; `[seen]` `8d665de7` s3: `layout.jsx` content starts with `<html lang="en">\n      <body>` (the indentation of the removed lines is still there); `6e0c3ebc`: build fails "Expression expected … layout.jsx:31". |
| **X5** | **"Next.js" is read as a file name.** The file-path rule matches `[\w-]+\.(…|js|…)`, so `Next.js` (also Node.js, Vue.js, Three.js, Express.js) becomes a path, and `fs_write` wrote the landing page to `/workspace/Next.js`. | `[seen]` `/workspace/Next.js`, 13,646 bytes, holds `15000756`'s first page; `[code]` `dfae-extract/src/rules.rs` file-path rule. |
| **X6** | **The syntax check is off for TS/TSX/JSX exactly when it matters.** `typescript_problem` needs `node_modules/typescript` above the file, and a page is written *before* `npm install` (or in a JS project without TS). So the truncated `layout.jsx` (X4) and the TS-only `as React.CSSProperties` in a `.jsx` file passed the check. They surfaced only as build failures. | `[code]` `syntax.rs:65-83`; `[seen]` every broken file in `6e0c3ebc` was written with "ok". |
| **X7** | **A content step that fails falls into the JSON binder with a 3-sample vote.** When `write_content` returns nothing, `fs_write` goes to `ask_for_args` with `max_tokens = WORKER_CONTENT_MAX_TOKENS` (16k) and the worker rung samples 3×: up to four full pages written inside JSON strings. | `[code]` `implement.rs:847` (vote) and `ask_for_args` (content limit); `[seen]` `8d665de7`: `worker:bind` 12.2k in / **11.4k out** and `planner:bind` 3.0k out, then "cannot determine the arguments of fs_write". |
| **X8** | **A probe command fails its step.** A shell step with `expect: null` that inspects ("`command -v node npm corepack nvm; npm --version`") exits non-zero when something is absent; that is a *finding*, but it fails the step, which triggers recovery and a re-plan. | `[seen]` `8085e1a8` r2_1: exit 127 → task failed. |
| **X9** | **Ports are never reclaimed or re-chosen.** A server from an earlier task still holds 8000; the planner wrote 8000 again; the clash went to the step agent, and the step agent had no budget left. | `[seen]` `next-server (v15.5.12)` from a previous task, still on 8000 after 31 min; `6e0c3ebc` s5. |

### Knowledge and prompts (quality)

| ID | Root cause | Evidence |
|---|---|---|
| **K1** | **The content contract is HTML-only.** `CONTENT_SYSTEM` says "a web page is a full HTML document with its styles inside it and no external files", and the rejection retry repeats "with its styles inside it and no external files". A Next.js `page.tsx` writer is told to inline everything, so the result is inline style objects (`as React.CSSProperties`) that ignore `globals.css`/Tailwind. That is where the poor quality and TS-in-JSX errors come from. | `[code]` `prompts.rs:121`, `implement.rs:638`; `[seen]` `6e0c3ebc` s3 content. |
| **K2** | **No scaffolding, and unpinned versions.** Every plan hand-wrote `package.json` with `"next": "latest"` (so no `tsconfig`, no `next.config`, no devDependencies, no lint) and got whatever was newest (Next 16.4.6). The worker's knowledge of that version is thin: it invented `--no-turbopack` and installed `@next/swc-wasm-napi` (404). The planner rule "files are written with fs_write … never with shell" pushes plans *away* from `create-next-app`. | `[seen]` `package.json` in all four tasks; `6e0c3ebc` s4_fix2/fix5; `[code]` `prompts.rs:46`. |
| **K3** | **Old projects are reused.** The rule "new work gets its own directory" is a prompt line only. The earth page, two crypto pages and two science pages all went into `site/`, so every attempt inherited the previous one's leftovers: `index.html`, `page.jsx` next to `page.tsx`, a stray `tsconfig.json`, a broken `layout.jsx`. Two of the twelve fix turns were spent deleting those. | `[seen]` `/workspace/site` today; `6e0c3ebc` s4_fix3, r1_1_fix5/6, r2_1. |
| **K4** | **The file writer cannot see the scaffold.** `related_files` shows only files *this task* wrote with `fs_write` (and files the step names). A page written into a generated project never sees `package.json`, `layout.tsx`, `globals.css` or `tsconfig.json`. | `[code]` `implement.rs:477-530`. |

### Recovery loop

| ID | Root cause | Evidence |
|---|---|---|
| **R1** | **The step agent repeats a failure it cannot change.** Four turns produced the identical `ld.so` line, each running a full `next build`. Six more were spent on r1_1. Twelve fix turns = 33k tokens = 46% of the task. | `[seen]` `6e0c3ebc` s4_fix1–6, r1_1_fix1–6 |
| **R2** | **Its history shows the head of each result.** `render_history` keeps `chars().take(2500)`, and a build log's error is at its end. It only worked because the model happened to add `\| tail -20`. | `[code]` `step_agent.rs` `render_history` |
| **R3** | **Fix turns draw on the task's tool budget.** The 25 tool calls were gone before the port clash (a one-line fix) could be tried. Time: the 600 s task budget ran out inside a planner call. | `[seen]` s5_fix1/2 "tool-call budget exceeded (25 allowed)"; "time budget exceeded during a Planner call" |
| **R4** | **Known environment failures have no hint.** `hint()` knows "command not found" and missing modules, not broken native modules (`ld.so`, `invalid ELF header`, `ERR_DLOPEN_FAILED`, `Bus error`) or a killed install. | `[code]` `coding.rs` `hint` |
| **R5** | **Transport errors end a re-plan.** Three attempts with 0.8 s/1.6 s backoff; a gateway that is away for ~10 s (it is, from this container, at times) ends the task. | `[seen]` `15000756`: "replanning failed: model request failed: error sending request"; container log: decisions API unreachable at 17:45 and 17:47 |

### Evaluation

| ID | Root cause | Evidence |
|---|---|---|
| **E1** | **The live Next.js case tests an easy version of the task.** Empty workspace, no serve, no page-quality check, and a warm cache. Real use has a persistent, dirty workspace, servers from earlier tasks, a 2 GB memory limit and a flaky gateway. 14/14 in the suite vs 0/4 in use. | `[code]` `live_e2e.rs:551-563` |

---

## 2. The plan

Order: **Phase A** first (the failures above all start there, and its fixes are 0-token code). Then B (quality), C (recovery), D
(proof). Each phase is independently shippable.

### Phase A — make the shell trustworthy (0 tokens, mostly < 50 lines each)

**A1. The tool's own timeout governs (X1).**
* Fix: the executor's outer timeout becomes `tool.max_runtime(&args)`, a new `Tool` trait method with default 120 s. The shell returns its
  effective timeout (`timeout_secs` or `default_timeout`) + 30 s grace; background starts return 60 s. Cap at the task's remaining time.
  On the outer timeout, return the partial output (`Capture` is already head+tail), not just "timed out".
* Files: `dfae-core/src/tool.rs`, `dfae-tools/src/native.rs` (`Shell`), `dfae-runtime/src/executor.rs:680`.
* Done when: a test runs a 150 s `sleep` with `timeout_secs=200` and it succeeds; a 700 s one is killed at its own limit with output.

**A2. A dropped command takes its whole process group with it (X2).**
* Fix: in `run_script`, hold a `GroupGuard(pid)` whose `Drop` calls `killpg(SIGKILL)`; disarm it after a normal `wait`. Also stop
  `kill_on_drop` from being the only defence for `spawn_job` on engine shutdown.
* Files: `dfae-tools/src/coding.rs`.
* Done when: a test drops a running `bash -c 'sleep 100 & sleep 100'` future and finds no `sleep` left.

**A3. An interrupted install is cleaned, not built on (X3, R4).**
* Fix: (a) when a command that matched `INSTALLER` times out or is cancelled, the output ends with
  `[interrupted install: node_modules may be incomplete — rm -rf node_modules && npm install]`; (b) `hint()` learns broken native modules:
  `Inconsistency detected by ld.so`, `invalid ELF header`, `ERR_DLOPEN_FAILED`, `wrong ELF class`, `Bus error` after `.node` →
  "a native module in node_modules is broken (usually an interrupted install): `rm -rf node_modules package-lock.json && npm install`".
* Files: `dfae-tools/src/coding.rs` (`hint`, `run_script`).
* Done when: unit tests on the four messages; live: a deliberately truncated `.node` file is repaired in ≤ 2 fix turns.

**A4. `strip_fences` touches HTML preambles only in HTML files (X4).**
* Fix: `strip_fences(text, path)`; the `<!doctype`/`<html` cut runs only for `.html`/`.htm`, and only when the removed prefix contains no
  code characters (`{ } ( ) ; =`, `export`, `import`, `function`, `return`).
* Files: `implement.rs` (+ both call sites).
* Done when: a regression test with the real `layout.jsx` from `8d665de7` keeps its first line.

**A5. Framework names are not files (X5).**
* Fix: in the file-path rule, reject a bare `Name.js|ts` (no `/`) whose stem is capitalised or is in a small list (`next, node, vue,
  nuxt, three, express, nest, react, d3, chart, socket, p5, alpine, ember, backbone, solid, svelte`) unless the file exists in the
  workspace. In `implement`, an `fs_write` path must come from the step's own sentence (`site/app/page.tsx`), not from the request.
* Files: `dfae-extract/src/rules.rs`, `implement.rs`.
* Done when: "landing page in the existing Next.js app" yields no file-path entity; tests for `notes.js`, `src/next.js` still pass.

**A6. TS/TSX/JSX are always parsed (X6).**
* Fix: parse in-process with the `oxc_parser` crate (pure Rust, parses TS/TSX/JSX/JS in well under 1 ms per file, no Node needed). Use the
  project's `typescript` only for type errors (already optional). Also flag TS syntax in `.js`/`.jsx` files ("`as` / type annotations in a
  JavaScript file"), and a `page`/`layout` file under `app/` with no default export.
* Files: `dfae-runtime/src/syntax.rs`, `dfae-runtime/Cargo.toml`.
* Done when: the truncated `layout.jsx` and the `as React.CSSProperties`-in-`.jsx` file from `6e0c3ebc` are both rejected at write time,
  so the content writer rewrites them (≈ 2k worker tokens) instead of a build + fix loop (≈ 15k).
* Risk: one more dependency (≈ 1–2 MB in the binary). The alternative is `npm i -g typescript` in the image (Node startup ~150 ms per check).

**A7. Content is never voted on, never JSON-bound (X7).**
* Fix: a step whose missing argument is a content parameter (`content`, `new_string`, `body`) and whose `write_content` failed retries
  `write_content` once, told why; then it fails with that reason. It never enters the `ask_for_args` ladder, and the 3-sample vote is
  disabled whenever the view contains a content parameter.
* Files: `implement.rs` (`implement_step` ladder).
* Done when: a scripted test where the content writer fails once spends exactly 2 content calls and 0 bind calls.
* Tokens: removes the worst case (≈ 15k output tokens in `8d665de7`).

**A8. A probe reports, it does not fail (X8).**
* Fix: a shell step with `expect == null` whose command has no state-changing verb (not an installer, not `rm/mv/cp/>`, not a build/test
  runner) succeeds with its output and `[exit code N]`; `evidence` carries the code. Builds, tests and installs keep failing on non-zero.
* Files: `executor.rs` (`run_tool`), a `probe_like(command)` helper in `implement.rs` beside `implies_background`.
* Done when: `command -v node npm nvm; npm --version` with nvm absent is a succeeded step; `npm run build` exiting 1 still fails.

**A9. Ports: reclaim our own, re-choose a taken one (X9).**
* Fix: before starting a background server on port P: if P is held by one of **our** jobs (`JOBS`) whose cwd is the same project, kill it
  (a redeploy); if it is held by anything else, rewrite `P` in the command (`--port`, `-p`, `PORT=`, `:P`) to the first free port in
  8000–8010 and rewrite the step's `expect` URL to match. Report the substitution in the step output.
* Files: `dfae-tools/src/coding.rs` (`refuse_taken_port` → `choose_port`), `executor.rs` (expect rewrite).
* Done when: with 8000 occupied by a foreign listener, a plan that says 8000 serves on 8001 and the answer gives 8001, with 0 re-plans.

### Phase B — knowledge where it is needed, and nowhere else (≈ +0 tokens on average)

The planner's system prompt stays byte-identical (the cache is worth 4.7× per call, `validation` C1). Everything below goes in the
**user message**, and only when it applies.

**B1. File-type-aware content contract (K1).**
* Fix: `CONTENT_SYSTEM` loses the HTML clause. The user message gets **one** contract line chosen by code from the path and the project:

  | Target | Contract line (≈ 30–60 tokens) |
  |---|---|
  | `.html` | full document, styles inline, no external files (today's rule) |
  | `app/**/page.tsx|jsx` in a Next project | a React Server Component with a default export; no `"use client"` unless it uses hooks/events; style with `className` and the project's CSS (Tailwind classes if `tailwindcss` is in `package.json`, otherwise `globals.css` / a CSS module); TypeScript only in `.ts(x)` |
  | `app/layout.tsx|jsx` | keep the root layout's `<html>/<body>`, `metadata` export and the `globals.css` import |
  | `globals.css` with Tailwind | keep `@import "tailwindcss"` (v4) / `@tailwind` directives (v3) as the first lines |
  | `.py`, `.rs`, `.go`, `.ts` (non-React) | today's "compiles or runs" |

  The rejection retry says only what was wrong, never "styles inside it".
* Files: `prompts.rs`, `implement.rs` (`write_content`), a small `content_contract(path, project)` fn.
* Done when: in 5 live runs, `page.tsx` has 0 inline `style={{…}}` objects for layout and uses the project's classes.

**B2. Recipes: the official generator, pinned (K2).**
* Fix: a table of deterministic **recipe cards** (in code, ~10 to start), each with a trigger (framework words in the request), a pinned
  scaffold command and the files the plan should then change:

  ```
  RECIPE nextjs (create-next-app 15, pinned): scaffold with
    `npx -y create-next-app@15 <dir> --ts --tailwind --eslint --app --src-dir --use-npm --no-import-alias --yes`
  then change src/app/page.tsx (and src/app/globals.css if needed), build with `npm run build` (cwd <dir>),
  serve with `npm run start -- -H 0.0.0.0 -p <port>` in the background. Never write package.json by hand.
  ```
  Cards: `nextjs`, `vite-react`, `vite-vue`, `astro`, `express`, `fastapi`, `flask`, `django`, `rust-cli`, `python-cli`.
  The card goes in the user message (≈ 90–130 tokens) only on a match. The rule "files are written with fs_write" gets the exception
  "a project is created by its generator". A plandoc lint flags `"latest"` (or no version) in a `package.json` a step writes.
  **Pinning a known major** (15 today) is the point: the worker knows it well and its flags are stable. A recipe is bumped deliberately,
  with a live run, not by whatever npm resolves today.
* Optional, 0 tokens: `DFAE_PREWARM=nextjs` runs the scaffold once at image build into the npm cache, so a scaffold takes ~15 s offline.
* Files: new `dfae-runtime/src/recipes.rs`, `pipeline.rs` (plan context), `prompts.rs:46`, `plandoc.rs` (lint), `Dockerfile`.
* Done when: a Next.js request's plan starts with the pinned scaffold in ≥ 9/10 runs.
* Tokens: −1 to −3k per task. `package.json`, `layout`, configs and lint setup are no longer written by the worker; only the page and
  styles are.

**B3. New work gets a new directory, enforced (K3).**
* Fix: a "create / build / make a new …" request whose plan writes into an existing, non-empty directory that the request does not name
  is repaired in code. The directory is renamed to a slug of the request (`lab-landing/`), and every path, cwd and expectation in the
  plan is rewritten. The planner is not called. The workspace snapshot marks directories with the task that made them
  (`site/ (by: "landing page about earth")`), so a follow-up ("change the hero of that page") still finds them.
* Files: `plandoc.rs` (repair), `workspace.rs`, `pipeline.rs`.
* Done when: three landing-page requests in a row produce three directories; "make its title bigger" edits the last one.

**B4. The writer sees the project (K4).**
* Fix: `related_files` adds, for a target inside a project (nearest `package.json`/`pyproject.toml`/`Cargo.toml`), a compact **project
  card**: the dependency names and majors, `tsconfig` paths, the layout's imports, the first 40 lines of `globals.css`
  (≈ 150–250 tokens). It replaces the cut-at-3500-chars dump of the last three written files when the card exists.
* Files: `implement.rs` (`related_files`), new `project_card(dir)`.
* Done when: a page written into a Tailwind v4 scaffold uses Tailwind classes, and one in a plain-CSS project uses `globals.css` classes.

### Phase C — a recovery loop that learns from its own turns

**C1. Same error twice: stop and escalate (R1).** Fingerprint each failed turn by its last complaint line, normalised (paths, numbers and
hashes masked). Two equal fingerprints end the loop: the planner gets "tried: <actions>; the same error every time: <line>".
Also stop on two turns whose command is the same after normalisation. *Tokens: −20 to −30k in `6e0c3ebc`-like runs.*

**C2. Show the end of the log (R2).** `render_history` uses `failure_digest` for failed results and head 600 + tail 1900 characters for
successful ones.

**C3. Own budget for fixing (R3).** Step-agent tool calls draw on `DFAE_STEP_AGENT_TOOL_CALLS` (default 12 per task) instead of the
task's 25. A task class "project" (a recipe matched, or the plan has a scaffold/build step) gets `max_seconds` 1500 and 40 tool calls.
Time is not tokens; model-call budgets stay as they are.

**C4. Patient re-plans on transport errors (R5).** A planner call made during recovery that fails with a transport error is retried
after 5 s, 15 s and 30 s (within the task's remaining time) before recovery gives up. Other calls keep today's fast retries.

**C5. Fix turns respect the write checks.** A heredoc or `python - <<EOF` script that writes a source file bypasses `fs_write`'s parse
check. After a shell fix turn, files it changed (mtime) are parsed with A6. A malformed one is reported in the turn result ("the script
left app/layout.jsx malformed: …").

### Phase D — prove it where it failed

**D1. A live case that looks like real use (E1).** Add `nextjs landing in a used workspace` to `live_e2e.rs`:
* before: `site/` holds an old `index.html`, a `page.jsx` and a broken `layout.jsx`; a foreign listener holds port 8000; cold npm cache;
* goal: "create a professional landing page about a scientific laboratory using nextjs and build and serve it";
* oracle: the answer's URL returns 200 with the lab's name; `npm run build` passes in the project directory; the project is **not**
  `site/`; no new file at the workspace root; `page.tsx` has a default export and uses `className`; no orphaned `npm`/`node` processes
  except the server; ≤ 1 re-plan; tokens ≤ 15k.
* Run it **inside the container** (`docker exec dfae-dfae-1 …`), 5 runs, interleaved with the old build, as the validation log does.

**D2. Regression tests from these records** (hermetic, scripted models): X4 (`layout.jsx`), X5 (`Next.js` path), X7 (content fallback),
X8 (probe), X9 (port), C1 (same error twice), using the real failing text from the four tasks.

---

## 3. Token economy: what changes

| Change | Planner tokens | Worker tokens |
|---|---|---|
| Phase A (all) | 0 | **−** (no JSON-bound pages; broken files caught at write time instead of in a build loop) |
| B1 contract line | 0 | +30–60 per content call |
| B2 recipe card | +90–130 in the user message, only on a match; the cached system prefix is unchanged | **−1 to −3k** (scaffold files no longer written) |
| B4 project card | 0 | +150–250 per content call in a project, offset by dropping the 3 × 3,500-char file dumps |
| C1 stop on a repeat | **−** (fewer, better-informed re-plans) | **−20 to −30k** on a stuck loop |

Expected for a Next.js landing page: plan ≈ 4k, page + styles ≈ 5k, binding and answer ≈ 1.5k, i.e. **≈ 10–12k tokens (≈ $0.003),
completed**. Today: 44k–80k tokens per attempt, and **failed**. Ordinary tasks (no recipe match, no project) see no change at all.

## 4. Order of work

1. **A1, A2, A4, A5, A9**: small, independent; together they remove the cascade that ended all four tasks.
2. **A6, A7, A8, C1, C2**: catch broken files early, stop paying for stuck loops.
3. **B2, B1, B3, B4**: quality: scaffolds, a contract per file type, clean project directories, a writer that sees the project.
4. **C3, C4, C5, A3**.
5. **D1, D2** run after step 1 and again after step 3. A change stays only if D1 improves (success, then tokens).

## 5. Open questions

* X3 is the best explanation for the `ld.so` crash but is not proven. A1 + A2 + A3 should make it disappear; D1 with a cold cache will
  show whether it does. If it recurs on a clean install, look next at the 2 GB `mem_limit` and the 1 GB `/tmp` tmpfs (counted against
  it) during `next build`.
* Pin Next 15 or move to 16? Pin the major the worker knows best (today 15), measure 16 in D1 before switching, and bump recipes deliberately.
* `oxc_parser` vs a global `typescript` in the image (A6): oxc is faster and needs no Node; `typescript` also gives type errors. Both can coexist.
