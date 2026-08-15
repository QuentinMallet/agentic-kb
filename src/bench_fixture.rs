//! Deterministic synthetic data shared by benchmarks and the CLI fixture builder.

use crate::components::{db, embedder::Embedder};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rusqlite::{params, Connection};
use serde_json::json;
use sha2::{Digest, Sha256};

pub const DEFAULT_SEED: u64 = 42;
pub const EMBED_DIM: usize = 384;
const POOL_SIZE: usize = 512;
const CATEGORIES: &[&str] = &["architecture", "gotchas", "debugging", "conventions", "runbooks", "antipatterns", "packages", "e2e", "security", "performance"];
const LOREM_WORDS: &[&str] = &["system", "module", "function", "trait", "struct", "impl", "async", "database", "index", "query", "cache", "latency", "throughput", "batch", "migration", "schema", "vector", "embedding", "similarity", "rank"];

pub struct BenchEmbedder { pool: Vec<Vec<f32>> }

impl BenchEmbedder {
    pub fn new(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let pool = (0..POOL_SIZE).map(|_| {
            let raw: Vec<f32> = (0..EMBED_DIM).map(|_| rng.gen::<f32>() * 2.0 - 1.0).collect();
            let norm = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
            raw.into_iter().map(|x| x / norm).collect()
        }).collect();
        Self { pool }
    }
}

impl Embedder for BenchEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut h = 0xcbf29ce484222325_u64;
        for byte in text.bytes() { h ^= u64::from(byte); h = h.wrapping_mul(0x100000003b4c61); }
        Ok(self.pool[h as usize % self.pool.len()].clone())
    }
    fn is_noop(&self) -> bool { false }
}

/// Populate entries, embeddings, cue anchors, and evidence through the runtime upsert path.
pub fn seed_db(conn: &Connection, emb: &BenchEmbedder, n: usize, seed: u64) -> anyhow::Result<()> {
    let mut rng = StdRng::seed_from_u64(seed);
    for i in 0..n {
        let cat = CATEGORIES[i % CATEGORIES.len()];
        let word = LOREM_WORDS[i % LOREM_WORDS.len()];
        let word2 = LOREM_WORDS[(i + 3) % LOREM_WORDS.len()];
        let hot = i % 100 == 0;
        let citation_path = if hot { "src/hot.rs:1-3" } else { "src/support.rs:1-3" };
        let event = json!({
            "action":"upsert", "table":"entries", "id":format!("bench-size-{i:07}"),
            "path":format!("bench/{cat}/entry-{i}"), "summary":format!("bench entry topic-{i} {cat}"),
            "content":format!("Entry {i} discusses {word} and {word2} in the context of {cat}. The {cat} subsystem relies on efficient {word} operations. Index {} provides {word2} guarantees under load.", i % 100),
            "tags":["bench",cat], "kind":"observation", "evidence_status":"missing",
            "permanent":false, "is_stale":false, "ts":"2024-01-01T00:00:00Z", "session_id":null,
            "cues":[format!("{cat} {word} cue {}", rng.gen::<u32>())]
        });
        db::apply_event(conn, emb, &event)?;
        conn.execute("INSERT INTO evidence(id,entry_id,kind,citation_path,citation_hash,citation_excerpt,recorded_at) VALUES(?1,?2,'code',?3,'sha256:fixture','alpha\\nbeta\\ngamma\\n','2024-01-01T00:00:00Z')",
            params![format!("ev-{i:07}"), format!("bench-size-{i:07}"), citation_path])?;
    }
    Ok(())
}

/// Stable logical digest, independent of SQLite page/WAL layout.
pub fn logical_checksum(conn: &Connection) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    for sql in [
        "SELECT id||'|'||path||'|'||summary||'|'||content||'|'||tags FROM entries ORDER BY id",
        "SELECT entry_id||'|'||cue||'|'||hex(embedding) FROM cues ORDER BY entry_id,id",
        "SELECT id||'|'||entry_id||'|'||citation_path||'|'||citation_hash FROM evidence ORDER BY id",
    ] {
        let mut stmt = conn.prepare(sql)?;
        for row in stmt.query_map([], |r| r.get::<_, String>(0))? { hasher.update(row?.as_bytes()); hasher.update(b"\n"); }
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_is_deterministic_for_same_seed() {
        let emb = BenchEmbedder::new(DEFAULT_SEED);
        let a = db::open_db_memory().unwrap(); let b = db::open_db_memory().unwrap();
        seed_db(&a, &emb, 32, DEFAULT_SEED).unwrap(); seed_db(&b, &emb, 32, DEFAULT_SEED).unwrap();
        assert_eq!(logical_checksum(&a).unwrap(), logical_checksum(&b).unwrap());
    }
}
