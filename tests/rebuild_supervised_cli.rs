#![cfg(target_os = "linux")]

use fs2::FileExt;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn proc_start_time(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| stat.split_whitespace().nth(21).map(str::to_owned))
}

struct ChildGuard {
    child: Child,
    stdout: BufReader<ChildStdout>,
    start_time: String,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none()
            && proc_start_time(self.child.id()).as_deref() == Some(self.start_time.as_str())
        {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn wait_for_exit(child: &mut ChildGuard) -> Option<std::process::ExitStatus> {
    let pid = child.child.id();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match child.child.try_wait() {
            Ok(Some(status))
                if proc_start_time(pid).as_deref() != Some(child.start_time.as_str()) =>
            {
                return Some(status)
            }
            Ok(Some(_)) => return None,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    None
}

fn read_frame(child: &mut ChildGuard) -> Value {
    let mut line = String::new();
    child.stdout.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn assert_no_stderr(child: &mut ChildGuard) {
    let mut stderr = String::new();
    child
        .child
        .stderr
        .as_mut()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.is_empty(),
        "supervised child stderr must be suppressed: {stderr:?}"
    );
}

fn start_supervised_rebuild(cwd: &Path, db: &Path) -> ChildGuard {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kb"))
        .args(["rebuild", "--db", db.to_str().unwrap(), "--supervised"])
        .current_dir(cwd)
        .env("KB_NO_EMBED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let start_time = proc_start_time(child.id()).expect("child must expose a Linux start time");
    ChildGuard {
        child,
        stdout,
        start_time,
    }
}

fn cancel_and_assert(child: &mut ChildGuard) {
    let stdin = child.child.stdin.as_mut().unwrap();
    stdin.write_all(&[0x03]).unwrap();
    stdin.flush().unwrap();
    let status = wait_for_exit(child);
    assert_eq!(status.and_then(|status| status.code()), Some(130));
    assert_no_stderr(child);
}

fn canonical_db(root: &Path) -> PathBuf {
    root.join(".state/agent-kb/agent-kb.db")
}

#[test]
fn supervised_rebuild_serializes_restart_contenders_for_the_selected_canonical_store() {
    let cwd = tempdir().unwrap();
    let selected = tempdir().unwrap();
    let selected_db = canonical_db(selected.path());
    let selected_lock = selected.path().join(".state/.lock");
    fs::create_dir_all(selected_lock.parent().unwrap()).unwrap();
    fs::create_dir_all(canonical_db(cwd.path()).parent().unwrap()).unwrap();

    // Keep normal rebuild work blocked after READY so this test isolates the
    // separate lifetime fence used across OTP owner restarts.
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(selected_lock)
        .unwrap();
    lock.lock_exclusive().unwrap();

    let mut first = start_supervised_rebuild(cwd.path(), &selected_db);
    assert_eq!(
        read_frame(&mut first),
        serde_json::json!({"rebuild": "ready"})
    );

    let mut contender = start_supervised_rebuild(cwd.path(), &selected_db);
    let error = read_frame(&mut contender);
    assert_eq!(error["rebuild"], "error");
    assert!(error["message"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
    assert!(wait_for_exit(&mut contender).is_some());
    assert_no_stderr(&mut contender);

    cancel_and_assert(&mut first);

    let mut replacement = start_supervised_rebuild(cwd.path(), &selected_db);
    assert_eq!(
        read_frame(&mut replacement),
        serde_json::json!({"rebuild": "ready"})
    );
    cancel_and_assert(&mut replacement);
}

#[test]
fn supervised_rebuild_binds_an_adjacent_legacy_store_and_accepts_eof() {
    let cwd = tempdir().unwrap();
    let selected = tempdir().unwrap();
    let selected_db = selected.path().join("agent-kb/agent-kb.db");
    let selected_events = selected.path().join("agent-kb/agent-kb-events.jsonl");
    fs::create_dir_all(selected_events.parent().unwrap()).unwrap();
    fs::write(selected_events, b"").unwrap();
    fs::create_dir_all(canonical_db(cwd.path()).parent().unwrap()).unwrap();

    let mut child = start_supervised_rebuild(cwd.path(), &selected_db);
    assert_eq!(
        read_frame(&mut child),
        serde_json::json!({"rebuild": "ready"})
    );
    drop(child.child.stdin.take());
    let status = wait_for_exit(&mut child);
    assert_eq!(status.and_then(|status| status.code()), Some(130));
    assert_no_stderr(&mut child);
}

#[test]
fn supervised_rebuild_reports_a_bounded_json_error_without_stderr() {
    let cwd = tempdir().unwrap();
    let impossible_component = "x".repeat(800);
    let db = cwd.path().join(impossible_component).join("agent-kb.db");
    let mut child = start_supervised_rebuild(cwd.path(), &db);
    let error = read_frame(&mut child);

    assert_eq!(error["rebuild"], "error");
    assert!(error["message"]
        .as_str()
        .is_some_and(|message| message.len() <= 512));
    assert!(wait_for_exit(&mut child).is_some());
    assert_no_stderr(&mut child);
}
