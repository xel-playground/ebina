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
///
/// `skill_chunks`/`skill_chunks_fts` are a genuinely separate pair of tables
/// (not a shared `chunks` with a `type` discriminator column) — adr/002 §2:
/// query logic doesn't need an extra filter on every call, and quota
/// separation (notes top-k, skills top 2-3) stays a table choice rather than
/// a `WHERE` clause. Same shape as `chunks`/`chunks_fts` since
/// `reindex_text_into`/`hybrid_search_table` below parametrize the table
/// names — one implementation, two independent tables.
pub fn ensure_schema() {
    for (table, fts_table) in [(CHUNKS_TABLE, CHUNKS_FTS_TABLE), (SKILL_CHUNKS_TABLE, SKILL_CHUNKS_FTS_TABLE)] {
        let _ = db_exec(
            &format!(
                "CREATE TABLE IF NOT EXISTS {table} (\
                    id INTEGER PRIMARY KEY, \
                    source_path TEXT NOT NULL, \
                    content_hash TEXT NOT NULL, \
                    text TEXT NOT NULL, \
                    embedding TEXT, \
                    embed_model TEXT\
                )"
            ),
            &Value::Array(vec![]),
        );
        let _ = db_exec(
            &format!("CREATE VIRTUAL TABLE IF NOT EXISTS {fts_table} USING fts5(text, content='', tokenize='porter unicode61')"),
            &Value::Array(vec![]),
        );
        let _ = db_exec(
            &format!("CREATE INDEX IF NOT EXISTS {table}_source_path_idx ON {table}(source_path)"),
            &Value::Array(vec![]),
        );
    }
}

const CHUNKS_TABLE: &str = "chunks";
const CHUNKS_FTS_TABLE: &str = "chunks_fts";
pub const SKILL_CHUNKS_TABLE: &str = "skill_chunks";
pub const SKILL_CHUNKS_FTS_TABLE: &str = "skill_chunks_fts";

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
/// Runs are per-session now, not serialized by one global lock (see
/// `kernel/src/gateway.rs`'s `AppState::session_locks`), so two concurrent
/// runs can genuinely both decide to reindex the *same* note at once. The
/// hash check + delete + insert used to be several independent `db_exec`
/// round-trips with no transaction around them — both runs could each
/// delete-then-insert their own copy, leaving duplicate rows for one
/// `source_path`. Fixed with optimistic concurrency: the (slow, network)
/// `embed` call happens *outside* any transaction so a concurrent run's own
/// reindex is never blocked waiting on it, then a `BEGIN IMMEDIATE`
/// transaction re-checks the hash right before writing — if another run
/// already reindexed this exact file to the same content while this one was
/// embedding, this one's now-redundant result is discarded instead of
/// inserted as a duplicate.
pub fn reindex_file(source_path: &str, embed_model: &str) -> Result<bool, String> {
    let text = std::fs::read_to_string(source_path).map_err(|e| format!("read {source_path}: {e}"))?;
    reindex_text(source_path, &text, embed_model)
}

/// The actual hash/chunk/embed/write logic, taking text directly instead of
/// reading `source_path` itself — adr/002-skill-v2.md §2: skills only want
/// their frontmatter description段 embedded, not the whole file, so the
/// caller extracts that text and hands it here rather than this function
/// re-reading the full SKILL.md. `reindex_file` above is now a thin wrapper
/// around this for notes (`reindex_all_notes` unchanged, still calls
/// `reindex_file`); `reindex_skill_text` below is the skill counterpart,
/// pointed at the separate `skill_chunks` table.
pub fn reindex_text(source_path: &str, text: &str, embed_model: &str) -> Result<bool, String> {
    reindex_text_into(source_path, text, embed_model, CHUNKS_TABLE, CHUNKS_FTS_TABLE)
}

/// Same as `reindex_text`, into `skill_chunks`/`skill_chunks_fts` instead of
/// `chunks`/`chunks_fts` — see `ensure_schema`'s doc comment for why these
/// are separate tables rather than one shared table with a `type` column.
pub fn reindex_skill_text(source_path: &str, text: &str, embed_model: &str) -> Result<bool, String> {
    reindex_text_into(source_path, text, embed_model, SKILL_CHUNKS_TABLE, SKILL_CHUNKS_FTS_TABLE)
}

fn reindex_text_into(source_path: &str, text: &str, embed_model: &str, table: &str, fts_table: &str) -> Result<bool, String> {
    let hash = content_hash(text);

    if same_as_indexed(source_path, &hash, embed_model, table)? {
        return Ok(false);
    }

    let pieces = chunk_markdown(text);
    if pieces.is_empty() {
        return Ok(false);
    }

    let embed_resp = syscall::call("embed", &serde_json::json!({"texts": pieces}));
    if embed_resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(format!("embed failed while reindexing {source_path}: {embed_resp}"));
    }
    let vectors = embed_resp["result"]["vectors"].as_array().cloned().unwrap_or_default();

    db_exec("BEGIN IMMEDIATE", &Value::Array(vec![]))?;
    if same_as_indexed(source_path, &hash, embed_model, table)? {
        let _ = db_exec("ROLLBACK", &Value::Array(vec![]));
        return Ok(false);
    }
    match write_reindexed_rows(source_path, &hash, embed_model, &pieces, &vectors, table, fts_table) {
        Ok(()) => {
            db_exec("COMMIT", &Value::Array(vec![]))?;
            Ok(true)
        }
        Err(e) => {
            let _ = db_exec("ROLLBACK", &Value::Array(vec![]));
            Err(e)
        }
    }
}

fn same_as_indexed(source_path: &str, hash: &str, embed_model: &str, table: &str) -> Result<bool, String> {
    let existing = db_exec(
        &format!("SELECT content_hash, embed_model FROM {table} WHERE source_path = ?1 LIMIT 1"),
        &serde_json::json!([source_path]),
    )?;
    Ok(existing.first().is_some_and(|row| {
        row.get("content_hash").and_then(|v| v.as_str()) == Some(hash) && row.get("embed_model").and_then(|v| v.as_str()) == Some(embed_model)
    }))
}

fn write_reindexed_rows(
    source_path: &str,
    hash: &str,
    embed_model: &str,
    pieces: &[String],
    vectors: &[Value],
    table: &str,
    fts_table: &str,
) -> Result<(), String> {
    let old_ids: Vec<i64> = db_exec(&format!("SELECT id FROM {table} WHERE source_path = ?1"), &serde_json::json!([source_path]))?
        .iter()
        .filter_map(|r| r.get("id").and_then(|v| v.as_i64()))
        .collect();
    for id in &old_ids {
        let _ = db_exec(&format!("DELETE FROM {fts_table} WHERE rowid = ?1"), &serde_json::json!([id]));
    }
    db_exec(&format!("DELETE FROM {table} WHERE source_path = ?1"), &serde_json::json!([source_path]))?;

    for (piece, vector) in pieces.iter().zip(vectors.iter()) {
        let embedding_json = vector.to_string();
        db_exec(
            &format!("INSERT INTO {table} (source_path, content_hash, text, embedding, embed_model) VALUES (?1,?2,?3,?4,?5)"),
            &serde_json::json!([source_path, hash, piece, embedding_json, embed_model]),
        )?;
        let id_rows = db_exec("SELECT last_insert_rowid() AS id", &Value::Array(vec![]))?;
        let new_id = id_rows.first().and_then(|r| r.get("id")).and_then(|v| v.as_i64()).unwrap_or(0);
        db_exec(
            &format!("INSERT INTO {fts_table}(rowid, text) VALUES (?1, ?2)"),
            &serde_json::json!([new_id, piece]),
        )?;
    }
    Ok(())
}

/// Reverse-orphan cleanup, the disk-side companion to the forward hash-diff
/// `reindex_text_into` already does — adr/001-memory-v2.md §5: merging or
/// deleting a note (or a skill) never used to clean its index row, so a
/// removed file's chunk stayed retrievable forever, pointing at a
/// `source_path` `read_file` would now fail on. Deletes every `table`/
/// `fts_table` row whose `source_path` isn't in `live_paths` (the set the
/// caller's own forward pass just enumerated from disk — no second directory
/// walk needed here). Returns how many distinct `source_path`s got cleaned,
/// for the same "don't do silent work" trace-line reasoning as
/// `reindex_all_notes`.
pub fn remove_orphaned_rows(table: &str, fts_table: &str, live_paths: &HashSet<String>) -> usize {
    let indexed: Vec<String> = db_exec(&format!("SELECT DISTINCT source_path FROM {table}"), &Value::Array(vec![]))
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get("source_path").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let mut removed = 0;
    for path in indexed {
        if live_paths.contains(&path) {
            continue;
        }
        let ids: Vec<i64> = db_exec(&format!("SELECT id FROM {table} WHERE source_path = ?1"), &serde_json::json!([path]))
            .unwrap_or_default()
            .iter()
            .filter_map(|r| r.get("id").and_then(|v| v.as_i64()))
            .collect();
        for id in &ids {
            let _ = db_exec(&format!("DELETE FROM {fts_table} WHERE rowid = ?1"), &serde_json::json!([id]));
        }
        let _ = db_exec(&format!("DELETE FROM {table} WHERE source_path = ?1"), &serde_json::json!([path]));
        removed += 1;
    }
    removed
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
    hybrid_search_table(query, top_k, CHUNKS_TABLE, CHUNKS_FTS_TABLE).into_iter().map(|(_, text)| text).collect()
}

/// Same retrieval, against `skill_chunks`/`skill_chunks_fts` — adr/002 §2's
/// separate quota (top 2-3, not notes' top-k). Returns `(source_path, text)`
/// pairs rather than just text: the caller needs `source_path` to map a hit
/// back to which skill it belongs to, for `retrieval_hit_count` bookkeeping
/// (§2: "自動檢索命中就 +1", tracked per skill file, not per chunk).
pub fn skill_search(query: &str, top_k: usize) -> Vec<(String, String)> {
    hybrid_search_table(query, top_k, SKILL_CHUNKS_TABLE, SKILL_CHUNKS_FTS_TABLE)
}

fn hybrid_search_table(query: &str, top_k: usize, table: &str, fts_table: &str) -> Vec<(String, String)> {
    let fts_query = sanitize_fts_query(query);
    let fts_ranks: HashMap<i64, usize> = if fts_query.is_empty() {
        HashMap::new()
    } else {
        db_exec(
            &format!("SELECT rowid AS id FROM {fts_table} WHERE {fts_table} MATCH ?1 ORDER BY bm25({fts_table}) LIMIT ?2"),
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
            let mut scored: Vec<(i64, f64)> = db_exec(&format!("SELECT id, embedding FROM {table}"), &Value::Array(vec![]))
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
            let row = db_exec(&format!("SELECT text, source_path FROM {table} WHERE id = ?1"), &serde_json::json!([id])).ok()?;
            let row = row.first()?;
            let text = row.get("text")?.as_str()?.to_string();
            let source_path = row.get("source_path")?.as_str()?.to_string();
            Some((source_path, text))
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
