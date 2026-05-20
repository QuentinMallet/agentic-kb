//! Event log operations (JSONL append + read)

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Append a single event to the JSONL log.
pub fn append_event(events_path: &Path, event: &serde_json::Value) -> Result<()> {
    if let Some(p) = events_path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(events_path)
        .with_context(|| format!("open events {}", events_path.display()))?;
    writeln!(f, "{}", serde_json::to_string(event)?)?;
    Ok(())
}

/// Read all events from a JSONL file.
pub fn read_events(events_path: &Path) -> Result<Vec<serde_json::Value>> {
    if !events_path.exists() {
        return Ok(vec![]);
    }
    let f = File::open(events_path)?;
    let reader = BufReader::new(f);
    let mut events = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("parse events line {}", i + 1))?;
        events.push(v);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_append_and_read_events_roundtrip() {
        let dir = tempdir().unwrap();
        let events_path = dir.path().join("events.jsonl");

        let e1 = serde_json::json!({"action": "upsert", "table": "entries", "id": "1"});
        let e2 = serde_json::json!({"action": "expire", "table": "entries", "id": "2"});
        let e3 = serde_json::json!({"action": "insert", "table": "run_history", "test_id": "t1"});

        append_event(&events_path, &e1).unwrap();
        append_event(&events_path, &e2).unwrap();
        append_event(&events_path, &e3).unwrap();

        let events = read_events(&events_path).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["id"], "1");
        assert_eq!(events[1]["id"], "2");
        assert_eq!(events[2]["test_id"], "t1");
    }

    #[test]
    fn test_read_events_nonexistent_file() {
        let events = read_events(Path::new("/tmp/nonexistent-events.jsonl")).unwrap();
        assert!(events.is_empty());
    }
}
