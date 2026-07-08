use crate::syscall;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const CHUNK_TARGET_CHARS: usize = 800;
const CHUNK_OVERLAP_CHARS: usize = 100;
const RRF_K: f64 = 60.0;
/// how many candidates each retrieval method contributes before RRF fusion
const CANDIDATES_PER_METHOD: usize = 50;

/// PROJECT.md 4.3: `chunks(source_path, content_hash, text, embedding, embed_model)`
/// + an FTS5 index for the keyword half of hybrid retrieval. Contentless FTS5
/// (`content=''`) — it only ever needs to answer "which rowids match this
/// query", never to hand back text, so there's no trigger-sync bookkeeping
/// to keep in step with `chunks`.
pub fn ensure_schema() {
    let _ = db_exec(
        "CREATE TABLE IF NOT EXISTS chunks (\
            id INTEGER PRIMARY KEY, \
            source_path TEXT NOT NULL, \
            content_hash TEXT NOT NULL, \
            text TEXT NOT NULL, \
            embedding TEXT, \
            embed_model TEXT\
        )",
        &Value::Array(vec![]),
    );
    let _ = db_exec(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, content='', tokenize='porter unicode61')",
        &Value::Array(vec![]),
    );
    let _ = db_exec(
        "CREATE INDEX IF NOT EXISTS chunks_source_path_idx ON chunks(source_path)",
        &Value::Array(vec![]),
    );
}

/// Splits markdown on header boundaries (`#`, `##`, ...); any section still
/// over `CHUNK_TARGET_CHARS` gets further sliced with `CHUNK_OVERLAP_CHARS`
/// of overlap so a fact split across a boundary doesn't vanish entirely from
/// either chunk.
pub fn chunk_markdown(text: &str) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.starts_with('#') && !current.trim().is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        sections.push(current);
    }

    let mut chunks = Vec::new();
    for section in sections {
        if section.trim().is_empty() {
            continue;
        }
        if section.len() <= CHUNK_TARGET_CHARS {
            chunks.push(section);
            continue;
        }
        let mut start = 0usize;
        while start < section.len() {
            let end = floor_char_boundary(&section, (start + CHUNK_TARGET_CHARS).min(section.len()));
            chunks.push(section[start..end].to_string());
            if end >= section.len() {
                break;
            }
            start = floor_char_boundary(&section, end.saturating_sub(CHUNK_OVERLAP_CHARS));
        }
    }
    chunks
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Crude scan of `/config.toml`'s `[embed]` section for `model = "..."` —
/// no toml parser in the guest, and the file's flat/known-shape enough that
/// this is simpler than pulling one in. Used to tag chunks with the model
/// that embedded them, so a config change triggers a full re-embed
/// (PROJECT.md 4.3: "embed_model 不符 → 自動全庫重嵌").
pub fn current_embed_model() -> String {
    let config_text = std::fs::read_to_string("/config.toml").unwrap_or_default();
    let mut in_embed_section = false;
    for line in config_text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_embed_section = line == "[embed]";
            continue;
        }
        if !in_embed_section {
            continue;
        }
        if let Some(rest) = line.strip_prefix("model").map(str::trim_start) {
            if let Some(value) = rest.strip_prefix('=') {
                return value.trim().trim_matches('"').to_string();
            }
        }
    }
    "unknown".to_string()
}

/// FNV-1a 64-bit — plenty for "did this file change since last index",
/// no crate needed
pub fn content_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Re-chunks and re-embeds `source_path` only if its content hash (or the
/// configured embed model) changed since last time — PROJECT.md 4.3's
/// "content hash 增量" / "embed_model 不符 → 自動全庫重嵌".
/// `Ok(true)` if it actually re-embedded the file, `Ok(false)` if the
/// content hash + embed model both matched what's already indexed and it
/// skipped — `agent_loop.rs::reindex_all_notes` sums this to report a
/// meaningful "reindexed N notes" trace line instead of silently doing
/// (or not doing) work every single run with nothing visible to show it.
pub fn reindex_file(source_path: &str, embed_model: &str) -> Result<bool, String> {
    let text = std::fs::read_to_string(source_path).map_err(|e| format!("read {source_path}: {e}"))?;
    let hash = content_hash(&text);

    let existing = db_exec(
        "SELECT content_hash, embed_model FROM chunks WHERE source_path = ?1 LIMIT 1",
        &serde_json::json!([source_path]),
    )?;
    if let Some(row) = existing.first() {
        let same_hash = row.get("content_hash").and_then(|v| v.as_str()) == Some(hash.as_str());
        let same_model = row.get("embed_model").and_then(|v| v.as_str()) == Some(embed_model);
        if same_hash && same_model {
            return Ok(false);
        }
    }

    let old_ids: Vec<i64> = db_exec("SELECT id FROM chunks WHERE source_path = ?1", &serde_json::json!([source_path]))?
        .iter()
        .filter_map(|r| r.get("id").and_then(|v| v.as_i64()))
        .collect();
    for id in &old_ids {
        let _ = db_exec("DELETE FROM chunks_fts WHERE rowid = ?1", &serde_json::json!([id]));
    }
    db_exec("DELETE FROM chunks WHERE source_path = ?1", &serde_json::json!([source_path]))?;

    let pieces = chunk_markdown(&text);
    if pieces.is_empty() {
        return Ok(false);
    }

    let embed_resp = syscall::call("embed", &serde_json::json!({"texts": pieces}));
    if embed_resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(format!("embed failed while reindexing {source_path}: {embed_resp}"));
    }
    let vectors = embed_resp["result"]["vectors"].as_array().cloned().unwrap_or_default();

    for (piece, vector) in pieces.iter().zip(vectors.iter()) {
        let embedding_json = vector.to_string();
        db_exec(
            "INSERT INTO chunks (source_path, content_hash, text, embedding, embed_model) VALUES (?1,?2,?3,?4,?5)",
            &serde_json::json!([source_path, hash, piece, embedding_json, embed_model]),
        )?;
        let id_rows = db_exec("SELECT last_insert_rowid() AS id", &Value::Array(vec![]))?;
        let new_id = id_rows.first().and_then(|r| r.get("id")).and_then(|v| v.as_i64()).unwrap_or(0);
        db_exec(
            "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
            &serde_json::json!([new_id, piece]),
        )?;
    }
    Ok(true)
}

/// FTS5 MATCH syntax breaks on punctuation/quotes in free-form user text —
/// keep only alnum + whitespace so it's always a safe implicit-AND query
fn sanitize_fts_query(query: &str) -> String {
    query
        .chars()
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Hybrid retrieval: FTS5 BM25 ranking + brute-force cosine ranking (fine at
/// this row count per PROJECT.md 4.3; sqlite-vec can replace the cosine loop
/// later without changing this function's contract), combined via
/// Reciprocal Rank Fusion. Returns up to `top_k` chunk texts, best first.
pub fn hybrid_search(query: &str, top_k: usize) -> Vec<String> {
    let fts_query = sanitize_fts_query(query);
    let fts_ranks: HashMap<i64, usize> = if fts_query.is_empty() {
        HashMap::new()
    } else {
        db_exec(
            "SELECT rowid AS id FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts) LIMIT ?2",
            &serde_json::json!([fts_query, CANDIDATES_PER_METHOD as i64]),
        )
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get("id").and_then(|v| v.as_i64()))
        .enumerate()
        .map(|(rank, id)| (id, rank))
        .collect()
    };

    let vec_ranks: HashMap<i64, usize> = embed_one(query)
        .map(|qvec| {
            let mut scored: Vec<(i64, f64)> = db_exec("SELECT id, embedding FROM chunks", &Value::Array(vec![]))
                .unwrap_or_default()
                .iter()
                .filter_map(|row| {
                    let id = row.get("id")?.as_i64()?;
                    let emb_text = row.get("embedding")?.as_str()?;
                    let emb: Vec<f64> = serde_json::from_str(emb_text).ok()?;
                    Some((id, cosine(&qvec, &emb)))
                })
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored
                .into_iter()
                .take(CANDIDATES_PER_METHOD)
                .enumerate()
                .map(|(rank, (id, _))| (id, rank))
                .collect()
        })
        .unwrap_or_default();

    let all_ids: HashSet<i64> = fts_ranks.keys().chain(vec_ranks.keys()).copied().collect();
    let mut combined: Vec<(i64, f64)> = all_ids
        .into_iter()
        .map(|id| {
            let mut score = 0.0;
            if let Some(rank) = fts_ranks.get(&id) {
                score += 1.0 / (RRF_K + *rank as f64 + 1.0);
            }
            if let Some(rank) = vec_ranks.get(&id) {
                score += 1.0 / (RRF_K + *rank as f64 + 1.0);
            }
            (id, score)
        })
        .collect();
    combined.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    combined.truncate(top_k);

    combined
        .into_iter()
        .filter_map(|(id, _)| {
            db_exec("SELECT text FROM chunks WHERE id = ?1", &serde_json::json!([id]))
                .ok()?
                .first()?
                .get("text")?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

fn embed_one(text: &str) -> Option<Vec<f64>> {
    let resp = syscall::call("embed", &serde_json::json!({"texts": [text]}));
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    resp["result"]["vectors"]
        .as_array()?
        .first()?
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect())
}

fn db_exec(sql: &str, params: &Value) -> Result<Vec<Value>, String> {
    let resp = syscall::call("db_exec", &serde_json::json!({"sql": sql, "params": params}));
    if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        Ok(resp["result"]["rows"].as_array().cloned().unwrap_or_default())
    } else {
        Err(resp
            .get("error")
            .map(|e| e.to_string())
            .unwrap_or_else(|| format!("db_exec failed for: {sql}")))
    }
}
