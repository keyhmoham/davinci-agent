//! Retrieval quality of the hash embedder against hosted embedding models, on paraphrases: what memory retrieval (skills,
//! lessons, facts) has to do when a later request says the same thing in other words. Ignored by default (it calls the gateway).
//!
//! ```bash
//! PLANNER_API_KEY=… cargo test -p dfae-memory --test embed_eval -- --ignored --nocapture
//! ```

use dfae_core::Embedder;
use dfae_memory::{HashEmbedder, cosine};

/// (what is stored, a later request that should find it)
const PAIRS: &[(&str, &str)] = &[
    ("create a landing page about earth in the site directory and serve it", "build a homepage about our planet and host it"),
    ("In config.toml change the port from 8080 to 9090", "update the listening port in the toml config to 9090"),
    ("Read changelog.md and tell me what changed in version 2", "what's new in release 2 according to the change log"),
    ("Which files mention TODO, and what is the first one about?", "find the files with todo notes and explain the first"),
    ("rename the function calcTotal to computeTotal across src", "refactor: give calcTotal the new name computeTotal everywhere"),
    ("the tests in test_stats.py fail, fix stats.py", "make the failing stats unit tests pass"),
    ("sum revenue per region in sales.csv and write totals.csv", "aggregate the sales spreadsheet by area into a totals file"),
    ("start a python http server for the site folder on port 8000", "serve the site directory over http"),
    ("install ffmpeg and convert video.mov to mp4", "transcode the mov clip into an mp4 file"),
    ("check disk usage on this machine", "how much storage space is left"),
    ("what is the weather in Paris right now", "current conditions in the French capital"),
    ("write unit tests for utils.py and run them", "add pytest coverage for the utilities module"),
    ("kill the process listening on port 8001", "stop whatever is bound to port 8001"),
    ("create a Next.js app called notes-app and build it", "scaffold a react next project for notes and compile it"),
    ("summarise the README", "give me a short overview of the readme file"),
    ("set up a FastAPI hello world on port 8002", "make a minimal python web api returning hello"),
    ("download data.csv from the URL into data/", "fetch the csv file from that link and save it in the data folder"),
    ("move all png files into images", "put every picture in the images directory"),
    ("show the last 20 lines of server.log", "tail the server log"),
    ("remember that my database server is db01.internal", "note: our db host is db01.internal"),
];

/// Mean similarity of the right match, and the mean and highest similarity of wrong ones: where a threshold can sit.
fn spread(sims: &[Vec<f32>]) -> (f32, f32, f32) {
    let n = sims.len() as f32;
    let pos = sims.iter().enumerate().map(|(i, r)| r[i]).sum::<f32>() / n;
    let negs: Vec<f32> = sims.iter().enumerate().flat_map(|(i, r)| r.iter().enumerate().filter(move |(j, _)| *j != i).map(|(_, v)| *v)).collect();
    let neg = negs.iter().sum::<f32>() / negs.len() as f32;
    let max_neg = negs.iter().cloned().fold(f32::MIN, f32::max);
    let min_pos = sims.iter().enumerate().map(|(i, r)| r[i]).fold(f32::MAX, f32::min);
    eprintln!("    right match: mean {pos:.2}, lowest {min_pos:.2}; wrong ones: mean {neg:.2}, highest {max_neg:.2}");
    (pos, neg, max_neg)
}

fn recall(sims: &[Vec<f32>], k: usize) -> f64 {
    let mut hits = 0;
    for (i, row) in sims.iter().enumerate() {
        let mut idx: Vec<usize> = (0..row.len()).collect();
        idx.sort_by(|a, b| row[*b].partial_cmp(&row[*a]).unwrap());
        if idx[..k].contains(&i) {
            hits += 1;
        }
    }
    hits as f64 / sims.len() as f64
}

async fn remote(model: &str, texts: &[&str]) -> Vec<Vec<f32>> {
    let key = std::env::var("PLANNER_API_KEY").expect("PLANNER_API_KEY");
    let v: serde_json::Value = reqwest::Client::new()
        .post("https://ai-gateway.vercel.sh/v1/embeddings")
        .bearer_auth(key)
        .json(&serde_json::json!({"model": model, "input": texts}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["data"].as_array().unwrap().iter().map(|d| d["embedding"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap() as f32).collect()).collect()
}

fn matrix(stored: &[Vec<f32>], queries: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let norm = |v: &[f32]| {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        v.iter().map(|x| x / n).collect::<Vec<f32>>()
    };
    queries.iter().map(|q| stored.iter().map(|s| cosine(&norm(q), &norm(s))).collect()).collect()
}

#[tokio::test]
#[ignore = "calls the gateway's embeddings endpoint"]
async fn paraphrase_retrieval_hash_vs_hosted_embeddings() {
    let stored: Vec<&str> = PAIRS.iter().map(|p| p.0).collect();
    let queries: Vec<&str> = PAIRS.iter().map(|p| p.1).collect();
    let h = HashEmbedder::default();
    let hs: Vec<Vec<f32>> = stored.iter().map(|t| h.embed(t)).collect();
    let hq: Vec<Vec<f32>> = queries.iter().map(|t| h.embed(t)).collect();
    let m = matrix(&hs, &hq);
    eprintln!("hash-256            recall@1 {:.2}  recall@3 {:.2}", recall(&m, 1), recall(&m, 3));
    spread(&m);
    for model in ["openai/text-embedding-3-small", "voyage/voyage-4-lite", "alibaba/qwen3-embedding-0.6b"] {
        let rs = remote(model, &stored).await;
        let rq = remote(model, &queries).await;
        let m = matrix(&rs, &rq);
        eprintln!("{model:<30} recall@1 {:.2}  recall@3 {:.2}", recall(&m, 1), recall(&m, 3));
        spread(&m);
        // the same request again, word for word (a repeat) and with one value changed (a skill's new parameters)
        let same = remote(model, &["read notes.txt and count its words", "read notes.txt and count its words", "read todo.md and count its words"]).await;
        let n = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let c = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>() / (n(a) * n(b));
        eprintln!("    identical {:.2}, one value changed {:.2}", c(&same[0], &same[1]), c(&same[0], &same[2]));
    }
}
