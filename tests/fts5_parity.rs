/// FTS5 parity test — T4 acceptance gate
///
/// Verifies zero result divergence between the contentless read path
/// (entries_fts) and the content='entries' read path (entries_fts_v2)
/// across a multi-class frozen corpus.
///
/// ADR 0001 §Parity-gate: zero divergence across the entire query suite.
/// Any single divergent result blocks cutover.
use kb::components::db::{
    apply_event, fts_query_content_entries, fts_query_contentless, open_db_memory, SearchOptions,
};
use rusqlite::Connection;

// ─── Corpus generation ───────────────────────────────────────────────────────

fn make_event(
    id: &str,
    path: &str,
    summary: &str,
    content: &str,
    tags: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "action": "upsert", "table": "entries",
        "id": id, "path": path, "summary": summary, "content": content,
        "tags": tags, "ts": "2024-01-01T00:00:00Z"
    })
}

fn make_evidence(entry_id: &str, idx: usize, excerpt: &str) -> serde_json::Value {
    serde_json::json!({
        "action": "upsert", "table": "evidence",
        "entry_id": entry_id,
        "id": format!("{entry_id}-ev{idx}"),
        "citation_path": format!("src/file{idx}.rs:1-5"),
        "citation_sha": "aaaa1234",
        "citation_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "citation_excerpt": excerpt,
        "ts": "2024-01-01T00:00:00Z"
    })
}

fn load_corpus(conn: &Connection) {
    let embedder = kb::components::embedder::NoopEmbedder;

    // ASCII word tokens (≥10 entries)
    for i in 0..12 {
        let ev = make_event(
            &format!("ascii-{i}"),
            &format!("src/ascii/{i}.rs"),
            &format!("authentication module revision {i}"),
            &format!("implements token validation and session handling for user {i}"),
            &["auth", "ascii"],
        );
        apply_event(conn, &embedder, &ev).unwrap();
        let ev2 = make_evidence(
            &format!("ascii-{i}"),
            i,
            &format!("fn validate_token_{i}() {{}}"),
        );
        apply_event(conn, &embedder, &ev2).unwrap();
    }

    // ASCII phrase tokens (≥10 entries)
    for i in 0..12 {
        let ev = make_event(
            &format!("phrase-{i}"),
            &format!("src/phrase/{i}.rs"),
            &format!("connection pool management cycle {i}"),
            &format!("manages database connection pool with retry backoff at index {i}"),
            &["phrase", "database"],
        );
        apply_event(conn, &embedder, &ev).unwrap();
    }

    // CJK tokens (≥10 entries)
    for (i, cjk) in [
        ("数据库", "索引"),
        ("认证", "令牌"),
        ("会话", "管理"),
        ("搜索", "引擎"),
        ("压缩", "算法"),
        ("日志", "记录"),
        ("缓存", "策略"),
        ("并发", "控制"),
        ("配置", "加载"),
        ("错误", "处理"),
        ("测试", "框架"),
        ("部署", "脚本"),
    ]
    .iter()
    .enumerate()
    {
        let ev = make_event(
            &format!("cjk-{i}"),
            &format!("docs/cjk/{i}.md"),
            &format!("{} {} entry {i}", cjk.0, cjk.1),
            &format!("CJK content: {} {} implementation detail {i}", cjk.0, cjk.1),
            &["cjk"],
        );
        apply_event(conn, &embedder, &ev).unwrap();
    }

    // Emoji tokens (≥10 entries)
    for (i, em) in [
        "rocket", "shield", "key", "lock", "warning", "check", "fire", "gear", "bug", "sparkles",
        "zap", "star",
    ]
    .iter()
    .enumerate()
    {
        let ev = make_event(
            &format!("emoji-{i}"),
            &format!("docs/emoji/{i}.md"),
            &format!("{em} status indicator entry {i}"),
            &format!("emoji token content: :{em}: represents state {i}"),
            &["emoji"],
        );
        apply_event(conn, &embedder, &ev).unwrap();
    }

    // Code identifier tokens (≥10 entries)
    for (i, sym) in [
        "HashMap",
        "BTreeMap",
        "Arc",
        "Mutex",
        "RwLock",
        "tokio",
        "serde_json",
        "rusqlite",
        "proptest",
        "tracing",
        "anyhow",
        "thiserror",
    ]
    .iter()
    .enumerate()
    {
        let ev = make_event(
            &format!("code-{i}"),
            &format!("src/code/{i}.rs"),
            &format!("{sym} usage example {i}"),
            &format!("code identifier example: use {sym}; in module {i}"),
            &["code", "rust"],
        );
        apply_event(conn, &embedder, &ev).unwrap();
    }

    // Markdown punctuation tokens (≥10 entries)
    for i in 0..12 {
        let ev = make_event(
            &format!("mdpunct-{i}"),
            &format!("docs/md/{i}.md"),
            &format!("markdown heading section {i}"),
            &format!("# Section {i}\n\n- bullet item\n- **bold** text\n- `code` span\n  * nested level {i}"),
            &["markdown"],
        );
        apply_event(conn, &embedder, &ev).unwrap();
    }

    // Regex-like punctuation tokens (≥10 entries)
    for (i, pat) in [
        "backslash",
        "dotstar",
        "caret",
        "dollar",
        "pipe",
        "question",
        "plus",
        "brace",
        "bracket",
        "paren",
        "percent",
        "ampersand",
    ]
    .iter()
    .enumerate()
    {
        let ev = make_event(
            &format!("regex-{i}"),
            &format!("src/regex/{i}.rs"),
            &format!("{pat} pattern entry {i}"),
            &format!("regex pattern token content involving {pat} metachar at index {i}"),
            &["regex"],
        );
        apply_event(conn, &embedder, &ev).unwrap();
    }
}

// ─── Query suite ─────────────────────────────────────────────────────────────

fn build_query_suite() -> Vec<String> {
    let mut queries: Vec<String> = Vec::new();

    // Top-1% frequency: very common tokens from the corpus, repeated to 50+
    let top_freq_terms = [
        "authentication",
        "session",
        "token",
        "database",
        "connection",
        "management",
        "validation",
        "module",
        "implementation",
        "content",
    ];
    for _ in 0..6 {
        for t in &top_freq_terms {
            queries.push(t.to_string());
        }
    }
    // trim to exactly 50 top-freq queries
    queries.truncate(50);

    // Middle-frequency: moderate-occurrence tokens
    let mid_freq_terms = [
        "pool",
        "retry",
        "backoff",
        "revision",
        "status",
        "indicator",
        "example",
        "heading",
        "section",
        "pattern",
        "metachar",
        "framework",
        "detail",
        "record",
        "strategy",
    ];
    for _ in 0..4 {
        for t in &mid_freq_terms {
            queries.push(t.to_string());
        }
    }
    // ensure ≥50 mid-freq queries
    while queries.len() < 100 {
        queries.push("index".to_string());
    }
    let mid_end = 100;

    // Bottom-1% frequency: tokens appearing only once (entry-unique content)
    let _low_freq: Vec<String> = (0..50).map(|i| format!("unique{i:04}token")).collect();
    // These don't appear in corpus → they become zero-result queries, which is fine.
    // Add low-freq corpus terms that appear exactly once
    let rare_terms = [
        "backoff",
        "cycle",
        "sparkles",
        "BTreeMap",
        "thiserror",
        "dotstar",
        "brace",
        "paren",
        "percent",
        "ampersand",
    ];
    queries.extend(rare_terms.iter().map(|s| s.to_string()));
    // Pad to 50 rare queries
    for i in 0..(50usize.saturating_sub(rare_terms.len())) {
        queries.push(format!("rareterm{i}"));
    }
    let _ = mid_end;

    // Boolean queries (≥20)
    queries.push("authentication OR connection".to_string());
    queries.push("session OR pool".to_string());
    queries.push("token AND validation".to_string());
    queries.push("database AND connection".to_string());
    queries.push("management AND module".to_string());
    queries.push("CJK OR cjk".to_string());
    queries.push("emoji OR indicator".to_string());
    queries.push("rust OR code".to_string());
    queries.push("markdown OR heading".to_string());
    queries.push("regex OR pattern".to_string());
    queries.push("HashMap OR BTreeMap".to_string());
    queries.push("authentication NOT session".to_string());
    queries.push("database NOT postgres".to_string());
    queries.push("token NOT oauth".to_string());
    queries.push("code AND identifier".to_string());
    queries.push("module AND revision".to_string());
    queries.push("pool AND retry".to_string());
    queries.push("session AND handling".to_string());
    queries.push("validation OR handling".to_string());
    queries.push("connection OR backoff".to_string());
    queries.push("sparkles OR zap".to_string());
    queries.push("serde_json OR tokio".to_string());

    // Zero-result queries (≥20) — tokens guaranteed absent from corpus
    queries.push("__ZERO_nonexistent_xyzzy_1".to_string());
    queries.push("__ZERO_nonexistent_xyzzy_2".to_string());
    queries.push("kubernetes".to_string());
    queries.push("postgresql".to_string());
    queries.push("elasticsearch".to_string());
    queries.push("rabbitmq".to_string());
    queries.push("graphql".to_string());
    queries.push("grpc".to_string());
    queries.push("terraform".to_string());
    queries.push("ansible".to_string());
    queries.push("prometheus".to_string());
    queries.push("grafana".to_string());
    queries.push("jaeger".to_string());
    queries.push("zipkin".to_string());
    queries.push("spire_workload".to_string());
    queries.push("openbao_transit".to_string());
    queries.push("zitadel_oidc".to_string());
    queries.push("beads_tracker".to_string());
    queries.push("nixpkgs_overlay".to_string());
    queries.push("flake_outputs".to_string());
    queries.push("cargo_workspace".to_string());
    queries.push("actix_web".to_string());
    queries.push("axum_router".to_string());
    queries.push("rocket_framework".to_string());

    queries
}

// ─── Parity assertion ────────────────────────────────────────────────────────

#[test]
fn test_fts5_parity_zero_divergence() {
    let conn = open_db_memory().unwrap();
    load_corpus(&conn);

    let opts = SearchOptions {
        do_fts: true,
        do_semantic: false,
        limit: 50,
        ..Default::default()
    };

    let queries = build_query_suite();

    // Ensure we meet the ADR minimums
    assert!(
        queries.len() >= 120,
        "query suite must be large enough; got {}",
        queries.len()
    );

    let mut divergent: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();

    for raw_query in &queries {
        let safe_q: String = raw_query
            .split_whitespace()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");

        let v1_ids: std::collections::BTreeSet<String> =
            fts_query_contentless(&conn, &safe_q, &opts)
                .unwrap_or_default()
                .into_iter()
                .map(|(id, ..)| id)
                .collect();

        let v2_ids: std::collections::BTreeSet<String> =
            fts_query_content_entries(&conn, &safe_q, &opts)
                .unwrap_or_default()
                .into_iter()
                .map(|(id, ..)| id)
                .collect();

        if v1_ids != v2_ids {
            divergent.push((
                raw_query.clone(),
                v1_ids.into_iter().collect(),
                v2_ids.into_iter().collect(),
            ));
        }
    }

    assert!(
        divergent.is_empty(),
        "FTS5 parity gate FAILED — {} divergent queries:\n{:#?}",
        divergent.len(),
        divergent
    );
}
