defmodule AgenticKbMcp.PortManagerTest do
  @moduledoc """
  Correlation, deadline, and crash-detection tests for `PortManager`
  (bd-21ef.2.8, ADR-3 in `.state/.omc/plans/c2-exclusion-boundary.md`).

  Uses `test/support/fake_port*.sh` in place of the real `kb mcp` Rust
  binary, so these tests exercise the Elixir side of the port protocol in
  isolation. Several tests are explicitly required (by the task spec) to
  fail against the pre-P1 implementation — see the comment on each.
  """

  use ExUnit.Case, async: false

  alias AgenticKbMcp.PortManager

  @fake_port Path.expand("support/fake_port.sh", __DIR__)
  @fake_port_db_error Path.expand("support/fake_port_db_error.sh", __DIR__)
  @fake_port_silent_crash Path.expand("support/fake_port_silent_crash.sh", __DIR__)

  setup_all do
    for path <- [@fake_port, @fake_port_db_error, @fake_port_silent_crash] do
      File.chmod!(path, 0o755)
    end

    :ok
  end

  defp unique_name(tag), do: :"pm_#{tag}_#{System.unique_integer([:positive, :monotonic])}"

  defp start_manager(kb_bin, tag) do
    name = unique_name(tag)

    pid =
      start_supervised!({PortManager, db_path: "unused", kb_bin: kb_bin, name: name}, id: name)

    {pid, name}
  end

  # ---------------------------------------------------------------------------
  # F2 regression: stale reply must not be delivered to the fresh request.
  # MUST FAIL pre-fix: the current collect_response/3 returns the first
  # final response it sees, with no id check at all (see CE1 in
  # PortProtocol-counterexample.md).
  # ---------------------------------------------------------------------------
  test "a stale reply ahead of the real one is discarded, not delivered as the answer" do
    {_pid, name} = start_manager(@fake_port, :stale)

    response =
      PortManager.call_port(%{"id" => "real-id", "method" => "stale_then_reply"}, 2_000, name)

    assert response["id"] == "real-id"
    assert response["type"] == "result"
  end

  # ---------------------------------------------------------------------------
  # Progress ticks must not be delivered as the final answer, and must not
  # reset the deadline. Covered together with the crash/deadline tests below;
  # this one just asserts a legitimate progress-then-reply exchange still
  # completes with the real reply.
  # ---------------------------------------------------------------------------
  test "progress ticks are consumed silently before the real reply" do
    {_pid, name} = start_manager(@fake_port, :progress)

    response =
      PortManager.call_port(%{"id" => "p-1", "method" => "progress_then_reply"}, 2_000, name)

    assert response == %{"id" => "p-1", "type" => "result"}
  end

  # ---------------------------------------------------------------------------
  # ADR-3 rule 3 (CE2): the inner deadline is absolute — an unbroken stream
  # of progress ticks must not extend it.
  # MUST FAIL pre-fix: the current collect_response recurses with the same
  # `timeout` value on every progress tick, which resets the per-receive
  # window (this is exactly CE2's counterexample), so this call would not
  # return within the asserted bound.
  # ---------------------------------------------------------------------------
  test "a continuous progress stream does not extend the absolute deadline" do
    {_pid, name} = start_manager(@fake_port, :progress_stream)

    started = System.monotonic_time(:millisecond)
    response = PortManager.call_port(%{"id" => "ps-1", "method" => "progress_stream"}, 400, name)
    elapsed = System.monotonic_time(:millisecond) - started

    assert response["type"] == "error"
    assert response["code"] == "timeout"
    # Generous slack above the 400ms budget — this is a liveness bound, not a
    # tight timing assertion.
    assert elapsed < 1_500
  end

  # ---------------------------------------------------------------------------
  # ADR-3 rules 2+3: a continuous stream of non-matching finals must also be
  # bounded by the absolute deadline (not the "first reply wins" bug), and
  # the timeout envelope must report how many were discarded.
  # MUST FAIL pre-fix: the current code has no id check at all, so it would
  # return the very first ("some-other-id") reply as if it were the answer —
  # `response["code"] == "timeout"` fails immediately.
  # ---------------------------------------------------------------------------
  test "a continuous stream of non-matching replies times out with a discard count" do
    {_pid, name} = start_manager(@fake_port, :discard_stream)

    started = System.monotonic_time(:millisecond)
    response = PortManager.call_port(%{"id" => "ds-1", "method" => "discard_stream"}, 400, name)
    elapsed = System.monotonic_time(:millisecond) - started

    assert response["type"] == "error"
    assert response["code"] == "timeout"
    assert is_integer(response["discarded_responses"])
    assert response["discarded_responses"] > 0
    assert elapsed < 1_500
  end

  # ---------------------------------------------------------------------------
  # ADR-3 rule 4 (two-timer race): GenServer.call must receive :infinity —
  # the caller's `timeout` is solely the inner deadline. A timed-out request
  # must return the timeout ENVELOPE, not crash the caller with a GenServer
  # `:exit` from its own outer call timeout.
  # MUST FAIL pre-fix: call_port passes the same `timeout` as both the inner
  # receive-after window AND GenServer.call's own timeout, so this races —
  # asserting the exact envelope shape below is not reliably satisfied.
  # ---------------------------------------------------------------------------
  test "a request that trips the inner deadline returns the timeout envelope, not a raised exit" do
    {_pid, name} = start_manager(@fake_port, :hang_timeout)

    response = PortManager.call_port(%{"id" => "h-1", "method" => "hang"}, 300, name)

    assert response["id"] == "h-1"
    assert response["type"] == "error"
    assert response["code"] == "timeout"
  end

  # ---------------------------------------------------------------------------
  # ADR-3 rule 1+5: killing the OS process (NOT Port.close/1) must be
  # observed promptly via :exit_status/trap_exit, well under the call
  # deadline, and reported as `port_closed` — not as a plain "timeout".
  # MUST FAIL pre-fix: init/1 never sets :exit_status or trap_exit, so a
  # killed OS process is invisible to collect_response; the call would only
  # ever resolve via its `after` timeout branch (code "timeout", not
  # "port_closed"), and only after the full budget elapses.
  # ---------------------------------------------------------------------------
  test "killing the port's OS process is observed promptly as port_closed" do
    {pid, name} = start_manager(@fake_port, :crash)

    %{port: port} = :sys.get_state(pid)
    {:os_pid, os_pid} = Port.info(port, :os_pid)

    call_timeout = 5_000

    task =
      Task.async(fn ->
        started = System.monotonic_time(:millisecond)
        response = PortManager.call_port(%{"id" => "c-1", "method" => "hang"}, call_timeout, name)
        {response, System.monotonic_time(:millisecond) - started}
      end)

    # Give the manager time to actually be blocked inside collect_response
    # for this call before killing the OS process out from under it.
    Process.sleep(100)
    System.cmd("kill", ["-9", to_string(os_pid)])

    {response, elapsed} = Task.await(task, call_timeout + 1_000)

    assert response["id"] == "c-1"
    assert response["type"] == "error"
    assert response["code"] == "port_closed"
    assert elapsed < 2_000
  end

  # ---------------------------------------------------------------------------
  # ADR-3 rule 5: a caller queued behind a request whose port crashes must
  # get a `port_unavailable` envelope, not a raised exit.
  # MUST FAIL pre-fix: call_port has no try/catch around GenServer.call, so
  # the exit signal delivered when the manager stops crashes the queued
  # caller's own process instead of returning an envelope.
  # ---------------------------------------------------------------------------
  test "a caller queued behind a crashing request gets a port_unavailable envelope" do
    {pid, name} = start_manager(@fake_port, :queued)

    %{port: port} = :sys.get_state(pid)
    {:os_pid, os_pid} = Port.info(port, :os_pid)

    first =
      Task.async(fn ->
        PortManager.call_port(%{"id" => "q-first", "method" => "hang"}, 5_000, name)
      end)

    # Ensure `first` is already being processed (the manager is blocked in
    # collect_response for it) before `second` is dispatched, so `second`
    # genuinely queues behind it in the GenServer's mailbox.
    Process.sleep(100)

    second =
      Task.async(fn ->
        PortManager.call_port(%{"id" => "q-second", "method" => "echo"}, 5_000, name)
      end)

    Process.sleep(50)
    System.cmd("kill", ["-9", to_string(os_pid)])

    assert %{"code" => "port_closed"} = Task.await(first, 5_000)

    second_response = Task.await(second, 5_000)
    assert second_response["type"] == "error"
    assert second_response["code"] == "port_unavailable"
  end

  # ---------------------------------------------------------------------------
  # ADR-3 rule 6: await_ready must surface a real startup error, not a bare
  # handshake_timeout. This one is already satisfied by the pre-fix code
  # (the {"type" => "error", ...} clause already exists) — it pins that
  # behavior against regression while the surrounding init/1 gains
  # :exit_status/trap_exit.
  # ---------------------------------------------------------------------------
  test "an unopenable-DB startup failure surfaces its real cause, not handshake_timeout" do
    Process.flag(:trap_exit, true)
    name = unique_name(:db_error)

    started = System.monotonic_time(:millisecond)

    result =
      GenServer.start_link(
        PortManager,
        [db_path: "unused", kb_bin: @fake_port_db_error, name: name],
        name: name
      )

    elapsed = System.monotonic_time(:millisecond) - started

    assert {:error, {:db_error, msg}} = result
    assert msg =~ "unable to open database"
    assert elapsed < 2_000
  end

  # ---------------------------------------------------------------------------
  # ADR-3 rule 6: a startup crash with NO protocol output at all must still
  # be observed immediately via :exit_status/:closed/:EXIT, not by waiting
  # out the full handshake_timeout (5s).
  # MUST FAIL pre-fix: init/1 never requests :exit_status and await_ready has
  # no clause for it, so this would hang for the full handshake_timeout and
  # report :handshake_timeout instead of the real (silent-crash) cause.
  # ---------------------------------------------------------------------------
  test "a silent startup crash is observed immediately, not via handshake_timeout" do
    Process.flag(:trap_exit, true)
    name = unique_name(:silent_crash)

    started = System.monotonic_time(:millisecond)

    result =
      GenServer.start_link(
        PortManager,
        [db_path: "unused", kb_bin: @fake_port_silent_crash, name: name],
        name: name
      )

    elapsed = System.monotonic_time(:millisecond) - started

    assert {:error, reason} = result
    refute reason == :handshake_timeout
    assert elapsed < 2_000
  end

  # ---------------------------------------------------------------------------
  # ADR-3 rule 2: request ids generated by the caller must be unique.
  # ---------------------------------------------------------------------------
  test "gen_id generates unique ids" do
    ids = for _ <- 1..1_000, do: AgenticKbMcp.McpServer.gen_id()
    assert length(Enum.uniq(ids)) == length(ids)
  end
end
