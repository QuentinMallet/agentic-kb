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
from pathlib import Path

KB_BIN, MCP_BIN = sys.argv[1:]
TIMEOUT = 5


def fail(message, proc=None):
    raise AssertionError(message)


def response(stream, expected_id):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        readable, _, _ = select.select([stream], [], [], max(0, deadline - time.monotonic()))
        if not readable:
            break
        line = stream.readline()
        if not line:
            break
        value = json.loads(line)
        if value.get("id") == expected_id:
            return value
    raise AssertionError(f"timed out waiting for JSON-RPC response id={expected_id}")


def request(proc, payload):
    proc.stdin.write(json.dumps(payload).encode() + b"\n")
    proc.stdin.flush()
    return response(proc.stdout, payload["id"])


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
        return Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ")
    except FileNotFoundError:
        return b""


def wait_for(predicate, message):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError(message)


def kill_process_group(proc):
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
    proc.wait(timeout=TIMEOUT)


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
        assert "result" in initialized, initialized
        fcntl.flock(lock_file, fcntl.LOCK_EX)
        started = request(proc, {
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "kb_rebuild", "arguments": {}},
        })
        assert "result" in started, started

        child_pid = wait_for(
            lambda: next((pid for pid in descendants(proc.pid)
                          if b"rebuild" in command_line(pid)
                          and proc_start_time(pid) is not None), None),
            "supervised rebuild child did not start",
        )
        child_start = proc_start_time(child_pid)
        assert child_start is not None

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

    subprocess.run(
        [KB_BIN, "add", "--path", "fixture/after", "--summary", "after",
         "--content", "after", "--tags", "fixture"],
        cwd=root,
        env=env,
        check=True,
        stdout=subprocess.DEVNULL,
    )
    subprocess.run([KB_BIN, "rebuild", "--db", str(db)], cwd=root, env=env, check=True,
                   stdout=subprocess.DEVNULL)
    search = subprocess.run([KB_BIN, "search", "after"], cwd=root, env=env, check=True,
                            stdout=subprocess.PIPE).stdout.decode()
    assert "fixture/after" in search, search
PY
