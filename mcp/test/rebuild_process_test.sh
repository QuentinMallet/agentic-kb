#!/usr/bin/env bash
set -euo pipefail

# Real temporary-store rebuild regression. Set KB_BIN and MCP_BIN to locally
# built artifacts; this test never opens a user knowledge-base store.
kb_bin=$(realpath "${KB_BIN:?set KB_BIN to the built Rust kb binary}")
mcp_bin=$(realpath "${MCP_BIN:?set MCP_BIN to the built MCP escript}")

exec python3 - "$kb_bin" "$mcp_bin" <<'PY'
import fcntl
import json
import os
import signal
import select
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

KB_BIN, MCP_BIN = sys.argv[1:]
TIMEOUT = 5


def fail(message, proc=None):
    if proc is None:
        raise AssertionError(message)
    readable, _, _ = select.select([proc.stderr], [], [], 0)
    stderr = os.read(proc.stderr.fileno(), 65_536) if readable else b""
    raise AssertionError(
        f"{message}; MCP exit={proc.poll()}; stderr={stderr.decode(errors='replace')!r}"
    )


def response(stream, expected_id):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        readable, _, _ = select.select([stream], [], [], max(0, deadline - time.monotonic()))
        if not readable:
            break
        line = stream.readline()
        if not line:
            break
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise AssertionError(f"non-JSON MCP stdout: {line!r}") from error
        if value.get("id") != expected_id:
            raise AssertionError(f"unexpected MCP response id: {value!r}")
        return value
    raise AssertionError(f"timed out waiting for JSON-RPC response id={expected_id}")


def request(proc, payload):
    proc.stdin.write(json.dumps(payload).encode() + b"\n")
    proc.stdin.flush()
    return response(proc.stdout, payload["id"])

def assert_success(response):
    assert "result" in response and "error" not in response, response
    assert response["result"].get("isError") is not True, response


def assert_tool_error(response):
    assert "result" in response and "error" not in response, response
    assert response["result"].get("isError") is True, response


def assert_no_unsolicited_stdout(proc):
    readable, _, _ = select.select([proc.stdout], [], [], 0.2)
    if readable:
        line = proc.stdout.readline()
        if line:
            raise AssertionError(f"unsolicited MCP stdout: {line!r}")

def proc_start_time(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().split()[21]
    except FileNotFoundError:
        return None


def direct_children(parent):
    children = []
    for candidate in Path("/proc").iterdir():
        if not candidate.name.isdigit():
            continue
        try:
            fields = (candidate / "stat").read_text().split()
            if int(fields[3]) == parent:
                children.append(int(candidate.name))
        except (FileNotFoundError, IndexError, ValueError):
            continue
    return children


def descendants(parent):
    pending = [parent]
    found = []
    while pending:
        children = direct_children(pending.pop())
        found.extend(children)
        pending.extend(children)
    return found


def command_line(pid):
    try:
        return Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")[:-1]
    except FileNotFoundError:
        return []


def rebuild_child(parent, db):
    expected = [os.fsencode(KB_BIN), b"rebuild", b"--db", os.fsencode(db), b"--supervised"]
    for pid in descendants(parent):
        try:
            executable = os.path.realpath(f"/proc/{pid}/exe")
        except FileNotFoundError:
            continue
        if executable == KB_BIN and command_line(pid) == expected and proc_start_time(pid):
            return pid
    return None


def wait_for(predicate, message):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError(message)

def lifetime_released(path):
    with open(path, "a+b") as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
    return True


def kill_process_group(proc):
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    if proc.poll() is None:
        proc.wait(timeout=TIMEOUT)


def append_committed_event(events):
    batch_id = str(uuid.uuid4())
    event = {
        "action": "upsert", "table": "entries", "id": f"managed-{batch_id}",
        "path": "fixture/after", "summary": "after", "content": "after",
        "tags": ["fixture"], "kind": "belief", "evidence_status": "n/a",
        "ts": "2026-09-13T00:00:00Z",
    }
    lines = [
        {"action": "batch_begin", "batch_id": batch_id, "n": 1},
        event,
        {"action": "batch_commit", "batch_id": batch_id, "n": 1},
    ]
    with events.open("ab") as handle:
        handle.write(b"".join(json.dumps(line).encode() + b"\n" for line in lines))
        handle.flush()
        os.fsync(handle.fileno())


with tempfile.TemporaryDirectory(prefix="mcp-rebuild-process.") as raw_root:
    root = Path(raw_root)
    db = root / ".state" / "agent-kb" / "agent-kb.db"
    db.parent.mkdir(parents=True)
    env = os.environ | {"KB_BIN": KB_BIN, "KB_DB_PATH": str(db), "KB_NO_EMBED": "1"}

    subprocess.run(
        [KB_BIN, "add", "--path", "fixture/before", "--summary", "before",
         "--content", "before", "--tags", "fixture"],
        cwd=root,
        env=env,
        check=True,
        stdout=subprocess.DEVNULL,
    )

    # The normal writer lock prevents rebuild work after its ready handshake,
    # leaving a real supervised child whose lifetime is tied to the MCP VM.
    lock_path = root / ".state" / ".lock"
    lock_file = lock_path.open("a+b")
    proc = subprocess.Popen(
        [MCP_BIN],
        cwd=root,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        initialized = request(proc, {
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "rebuild-process", "version": "1"}},
        })
        assert_success(initialized)
        fcntl.flock(lock_file, fcntl.LOCK_EX)
        started = request(proc, {
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "kb_rebuild", "arguments": {}},
        })
        assert_success(started)
        readable = request(proc, {
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "kb_search", "arguments": {"query": "before"}},
        })
        assert_success(readable)

        child_pid = wait_for(
            lambda: rebuild_child(proc.pid, str(db)),
            "supervised rebuild child did not start with the selected DB",
        )
        child_start = proc_start_time(child_pid)
        assert child_start is not None

        # Append one committed event while the writer lock is held. A second
        # `kb add` would materialize it itself and make this test vacuous.
        events = db.parent / "agent-kb-events.jsonl"
        append_committed_event(events)
        absent = request(proc, {
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "kb_search", "arguments": {"query": "after"}},
        })
        assert_success(absent)
        assert "fixture/after" not in json.dumps(absent), absent

        fence = root / ".state" / ".lock.rebuild-lifetime.lock"
        assert not lifetime_released(fence), "managed rebuild lifetime fence was not held"
        fcntl.flock(lock_file, fcntl.LOCK_UN)
        wait_for(lambda: lifetime_released(fence), "managed rebuild did not complete")
        wait_for(lambda: proc_start_time(child_pid) != child_start,
                 "managed rebuild child did not exit")
        rebuild_log = db.parent / "rebuild.log"
        wait_for(rebuild_log.exists, "managed rebuild completion was not observed")
        visible = request(proc, {
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "kb_search", "arguments": {"query": "after"}},
        })
        assert_success(visible)
        assert "fixture/after" in json.dumps(visible), visible
        assert_no_unsolicited_stdout(proc)

        # A busy lifetime fence is an MCP tool error, and its nonzero worker
        # diagnostic must not leak through the JSON-RPC stdout stream.
        with fence.open("a+b") as busy_lock:
            fcntl.flock(busy_lock, fcntl.LOCK_EX)
            failed = request(proc, {
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": {"name": "kb_rebuild", "arguments": {}},
            })
            assert_tool_error(failed)
            wait_for(lambda: "acquire supervised rebuild lifetime lock" in
                     rebuild_log.read_text(), "busy rebuild did not report its error")
            assert_no_unsolicited_stdout(proc)

        still_visible = request(proc, {
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "kb_search", "arguments": {"query": "after"}},
        })
        assert_success(still_visible)
        assert "fixture/after" in json.dumps(still_visible), still_visible

        fcntl.flock(lock_file, fcntl.LOCK_EX)
        restarted = request(proc, {
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": {"name": "kb_rebuild", "arguments": {}},
        })
        assert_success(restarted)
        child_pid = wait_for(
            lambda: rebuild_child(proc.pid, str(db)),
            "second supervised rebuild child did not start with the selected DB",
        )
        child_start = proc_start_time(child_pid)

        # A whole-VM kill must close the port stdin and reap the direct rebuild
        # child even while ordinary rebuild work is blocked on the writer lock.
        vm_executable = os.readlink(f"/proc/{proc.pid}/exe")
        assert Path(vm_executable).name.startswith("beam"), vm_executable
        os.kill(proc.pid, signal.SIGKILL)
        proc.wait(timeout=TIMEOUT)
        wait_for(
            lambda: proc_start_time(child_pid) != child_start,
            "supervised rebuild child survived whole MCP VM termination",
        )
    except Exception as error:
        kill_process_group(proc)
        fail(str(error), proc)
    finally:
        lock_file.close()
PY
