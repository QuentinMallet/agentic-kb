#![cfg(target_os = "linux")]

use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

fn wait_for_death(child: &mut ChildGuard) -> bool {
    let pid = child.child.id();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match child.child.try_wait() {
            Ok(Some(_)) => {
                return proc_start_time(pid).as_deref() != Some(child.start_time.as_str())
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
    false
}

fn start_supervised_rebuild(cwd: &Path, db: &Path) -> ChildGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_kb"))
        .args(["rebuild", "--db", db.to_str().unwrap(), "--supervised"])
        .current_dir(cwd)
        .env("KB_NO_EMBED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start_time = proc_start_time(child.id()).expect("child must expose a Linux start time");
    ChildGuard { child, start_time }
}

fn assert_uses_selected_lock_and_exits_on_eof(cwd: &Path, selected_db: &Path, _lock: &File) {
    let mut child = start_supervised_rebuild(cwd, selected_db);

    thread::sleep(Duration::from_millis(300));
    assert!(
        child.child.try_wait().unwrap().is_none(),
        "the selected store lock must keep the supervised child alive while stdin remains open"
    );

    drop(child.child.stdin.take());
    let exited = wait_for_death(&mut child);
    assert!(
        exited,
        "closing the OTP-owned stdin pipe must end the supervised rebuild child"
    );
}

fn canonical_db(root: &Path) -> PathBuf {
    root.join(".state/agent-kb/agent-kb.db")
}

#[test]
fn supervised_rebuild_binds_the_explicit_canonical_store_and_exits_on_eof() {
    let cwd = tempdir().unwrap();
    let selected = tempdir().unwrap();
    let selected_db = canonical_db(selected.path());
    let selected_lock = selected.path().join(".state/.lock");
    fs::create_dir_all(selected_lock.parent().unwrap()).unwrap();
    fs::create_dir_all(canonical_db(cwd.path()).parent().unwrap()).unwrap();

    let lock = OpenOptions::new().create(true).read(true).write(true).open(selected_lock).unwrap();
    lock.lock_exclusive().unwrap();
    assert_uses_selected_lock_and_exits_on_eof(cwd.path(), &selected_db, &lock);
}

#[test]
fn supervised_rebuild_binds_an_explicit_adjacent_legacy_store_and_exits_on_eof() {
    let cwd = tempdir().unwrap();
    let selected = tempdir().unwrap();
    let selected_db = selected.path().join("agent-kb/agent-kb.db");
    let selected_lock = selected.path().join("agent-kb/agent-kb.lock");
    fs::create_dir_all(selected_lock.parent().unwrap()).unwrap();
    fs::create_dir_all(canonical_db(cwd.path()).parent().unwrap()).unwrap();

    let lock = OpenOptions::new().create(true).read(true).write(true).open(selected_lock).unwrap();
    lock.lock_exclusive().unwrap();
    assert_uses_selected_lock_and_exits_on_eof(cwd.path(), &selected_db, &lock);
}
