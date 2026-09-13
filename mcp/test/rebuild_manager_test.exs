defmodule AgenticKbMcp.RebuildManagerTest do
  use ExUnit.Case, async: false

  alias AgenticKbMcp.{PortManager, RebuildManager}

  @fixture Path.expand("support/rebuild_lifecycle_fake.sh", __DIR__)
  @mcp_fixture Path.expand("support/fake_port.sh", __DIR__)
  @log_limit 65_536

  setup do
    File.chmod!(@fixture, 0o755)
    root = Path.join(System.tmp_dir!(), "mcp-rebuild-#{System.unique_integer([:positive])}")
    File.mkdir_p!(root)

    pid_file = Path.join(root, "rebuild.pid")
    args_file = Path.join(root, "rebuild.args")
    launch_file = Path.join(root, "rebuild.launches")
    completed_file = Path.join(root, "rebuild.completed")
    control_file = Path.join(root, "rebuild.control")
    db_path = Path.join([root, ".state", "agent-kb", "agent-kb.db"])
    File.mkdir_p!(Path.dirname(db_path))
    File.write!(db_path, "")

    on_exit(fn -> File.rm_rf!(root) end)

    %{
      root: root,
      pid_file: pid_file,
      args_file: args_file,
      launch_file: launch_file,
      completed_file: completed_file,
      control_file: control_file,
      rebuild_log: Path.join(Path.dirname(db_path), "rebuild.log"),
      db_path: db_path
    }
  end

  test "launch is synchronous, coalesces an active child, and binds the active database", ctx do
    {manager, _pid} = start_manager(ctx, :hold)

    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    first = await_live_identity(ctx.launch_file)
    assert worker_alive?(first)
    assert File.read!(ctx.args_file) =~ "rebuild --db #{ctx.db_path} --supervised"

    assert {:ok, :already_running} = RebuildManager.request_rebuild(manager)
    assert launch_count(ctx.launch_file) == 1
    assert worker_alive?(first)
    assert :ok = RebuildManager.cancel_and_await(manager, 2_000)
  end

  test "private cancellation waits for observed OS exit before a retry can launch", ctx do
    {manager, _pid} = start_manager(ctx, :hold)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    first = await_live_identity(ctx.launch_file)
    assert worker_alive?(first)

    assert :ok = RebuildManager.cancel_and_await(manager, 2_000)
    assert File.read!(ctx.control_file) == "cancel-byte\n"
    refute worker_alive?(first)
    assert launch_count(ctx.launch_file) == 1
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    assert_eventually(fn -> launch_count(ctx.launch_file) == 2 end)
    second = await_live_identity(ctx.launch_file)
    assert second != first
    assert worker_alive?(second)
    assert :ok = RebuildManager.cancel_and_await(manager, 2_000)
  end

  test "a cancellation timeout keeps the rebuild slot occupied with an explicit error", ctx do
    {manager, _pid} = start_manager(ctx, :hold)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    assert {:error, :timeout} = RebuildManager.cancel_and_await(manager, 1)
    assert %{phase: :unknown} = :sys.get_state(manager)

    assert {:ok, %{exit_status: 130}} = RebuildManager.await_terminal(manager, 2_000)
  end

  test "a closed port without exit status releases waiters and permits an explicit lock-gated retry",
       ctx do
    {manager, _pid} = start_manager(ctx, :hold)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    %{port: port} = :sys.get_state(manager)

    waiter = Task.async(fn -> RebuildManager.await_terminal(manager, 2_000) end)
    send(manager, {port, :closed})

    assert {:error, :exit_status_missing} = Task.await(waiter, 2_000)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    assert_eventually(fn -> launch_count(ctx.launch_file) == 2 end)
    %{port: replacement} = :sys.get_state(manager)
    send(manager, {port, {:exit_status, 99}})
    assert %{port: ^replacement, phase: :running} = :sys.get_state(manager)
    assert :ok = RebuildManager.cancel_and_await(manager, 2_000)
  end

  test "an executable launch failure is returned rather than acknowledged as started", ctx do
    missing = Path.join(ctx.root, "missing-kb")
    {manager, _pid} = start_manager(ctx, :hold, kb_bin: missing)

    assert {:error, {:launch_failed, _reason}} = RebuildManager.request_rebuild(manager)
    refute File.exists?(ctx.rebuild_log)
  end

  test "a pre-ready busy error is returned instead of a false started acknowledgment", ctx do
    {manager, _pid} = start_manager(ctx, :pre_ready_error)

    assert {:error, {:launch_failed, "busy"}} = RebuildManager.request_rebuild(manager)
    assert {:ok, %{exit_status: 75}} = RebuildManager.await_terminal(manager, 2_000)
  end

  test "a child that never becomes ready returns a launch timeout and remains occupied", ctx do
    {manager, _pid} = start_manager(ctx, :no_ready)

    assert {:error, :ready_timeout} = RebuildManager.request_rebuild(manager)
    assert %{phase: :unknown} = :sys.get_state(manager)
    %{port: port} = :sys.get_state(manager)
    Port.close(port)
    assert_eventually(fn -> File.exists?(ctx.completed_file) end)
  end

  test "a concurrent request cannot coalesce before READY", ctx do
    {manager, _pid} = start_manager(ctx, :delayed_ready)
    launch = Task.async(fn -> RebuildManager.request_rebuild(manager) end)

    assert_eventually(fn -> :sys.get_state(manager).phase == :starting end)
    assert {:error, :rebuild_starting} = RebuildManager.request_rebuild(manager)
    assert {:error, :ready_timeout} = Task.await(launch, 2_000)
    assert %{phase: :unknown} = :sys.get_state(manager)
    assert {:ok, %{exit_status: 130}} = RebuildManager.await_terminal(manager, 2_000)
    assert File.read!(ctx.control_file) == "cancel-byte\n"
  end

  test "a pre-READY close resolves the blocked launch request", ctx do
    {manager, _pid} = start_manager(ctx, :no_ready)
    launch = Task.async(fn -> RebuildManager.request_rebuild(manager) end)

    assert_eventually(fn -> :sys.get_state(manager).phase == :starting end)
    %{port: port} = :sys.get_state(manager)
    send(manager, {port, :closed})

    System.put_env("REBUILD_FIXTURE_MODE", "hold")
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    assert {:error, {:launch_failed, :rebuild_status_unknown}} = Task.await(launch, 2_000)
    assert :ok = RebuildManager.cancel_and_await(manager, 2_000)
  end

  test "a nonzero child exit is observed asynchronously and retained in the log", ctx do
    {manager, _pid} = start_manager(ctx, :fail)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)

    failed = await_captured_identity(ctx.launch_file)

    assert {:ok, %{exit_status: 42, log_tail: tail}} =
             RebuildManager.await_terminal(manager, 2_000)

    refute worker_alive?(failed)
    assert tail =~ "controlled rebuild failure"
    assert File.read!(ctx.rebuild_log) =~ "controlled rebuild failure"
  end

  test "a log persistence error does not strand terminal waiters", ctx do
    {manager, _pid} = start_manager(ctx, :fail, db_path: "/dev/null/agent-kb.db")
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)

    assert {:ok, %{exit_status: 42, log_tail: tail}} =
             RebuildManager.await_terminal(manager, 2_000)

    assert tail =~ "controlled rebuild failure"
    assert %{phase: :idle} = :sys.get_state(manager)
  end

  test "captured rebuild output is bounded", ctx do
    {manager, _pid} = start_manager(ctx, :flood)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)

    log = ctx.rebuild_log
    flooded = await_captured_identity(ctx.launch_file)

    assert {:ok, %{exit_status: 0, log_tail: tail}} =
             RebuildManager.await_terminal(manager, 2_000)

    refute worker_alive?(flooded)
    assert tail =~ "TAIL: flood-complete"
    assert File.stat!(log).size <= @log_limit
  end

  test "a fresh request starts only after natural completion has been observed", ctx do
    {manager, _pid} = start_manager(ctx, :flood)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    assert {:ok, %{exit_status: 0}} = RebuildManager.await_terminal(manager, 2_000)

    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    assert_eventually(fn -> launch_count(ctx.launch_file) == 2 end)
    assert :ok = RebuildManager.cancel_and_await(manager, 2_000)
  end

  test "ordinary MCP-port requests remain responsive while rebuild holds its lock", ctx do
    {manager, _pid} = start_manager(ctx, :hold)
    port_name = :"rebuild_port_#{System.unique_integer([:positive, :monotonic])}"
    File.chmod!(@mcp_fixture, 0o755)

    start_supervised!({PortManager, db_path: ctx.db_path, kb_bin: @mcp_fixture, name: port_name})

    assert {:ok, :started} = RebuildManager.request_rebuild(manager)

    assert %{"id" => "ordinary-read", "type" => "result"} =
             PortManager.call_port(
               %{"id" => "ordinary-read", "method" => "echo"},
               1_000,
               port_name
             )

    assert :ok = RebuildManager.cancel_and_await(manager, 2_000)
  end

  defp start_manager(ctx, mode, opts \\ []) do
    name = :"rebuild_#{System.unique_integer([:positive, :monotonic])}"

    previous = System.get_env("REBUILD_FIXTURE_MODE")
    previous_eof_delay = System.get_env("REBUILD_EOF_EXIT_DELAY")
    System.put_env("REBUILD_FIXTURE_MODE", Atom.to_string(mode))
    System.put_env("REBUILD_EOF_EXIT_DELAY", "0.2")
    System.put_env("REBUILD_PID_FILE", ctx.pid_file)
    System.put_env("REBUILD_ARGS_FILE", ctx.args_file)
    System.put_env("REBUILD_LAUNCH_FILE", ctx.launch_file)
    System.put_env("REBUILD_COMPLETED_FILE", ctx.completed_file)
    System.put_env("REBUILD_CONTROL_FILE", ctx.control_file)

    on_exit(fn ->
      restore_env("REBUILD_FIXTURE_MODE", previous)
      restore_env("REBUILD_EOF_EXIT_DELAY", previous_eof_delay)
      System.delete_env("REBUILD_PID_FILE")
      System.delete_env("REBUILD_ARGS_FILE")
      System.delete_env("REBUILD_LAUNCH_FILE")
      System.delete_env("REBUILD_COMPLETED_FILE")
      System.delete_env("REBUILD_CONTROL_FILE")
    end)

    kb_bin = Keyword.get(opts, :kb_bin, @fixture)
    db_path = Keyword.get(opts, :db_path, ctx.db_path)
    {name, start_supervised!({RebuildManager, db_path: db_path, kb_bin: kb_bin, name: name})}
  end

  defp restore_env(key, nil), do: System.delete_env(key)
  defp restore_env(key, value), do: System.put_env(key, value)

  defp await_live_identity(launch_file) do
    assert_eventually(fn ->
      case captured_identity(launch_file) do
        {:ok, identity} -> worker_alive?(identity)
        _ -> false
      end
    end)

    {:ok, identity} = captured_identity(launch_file)
    identity
  end

  defp await_captured_identity(launch_file) do
    assert_eventually(fn -> match?({:ok, _identity}, captured_identity(launch_file)) end)
    {:ok, identity} = captured_identity(launch_file)
    identity
  end

  defp worker_alive?(%{pid: pid, start_time: start_time}) do
    case File.read("/proc/#{pid}/stat") do
      {:ok, stat} ->
        with [_comm, rest] <- String.split(stat, ")", parts: 2),
             [state | fields] <- String.split(rest, ~r/\s+/, trim: true),
             ^start_time <- Enum.at(fields, 18) do
          state != "Z"
        else
          _ -> false
        end

      _ ->
        false
    end
  end

  defp captured_identity(path) do
    with {:ok, contents} <- File.read(path),
         line when is_binary(line) <- contents |> String.split("\n", trim: true) |> List.last(),
         [pid, start_time] <- String.split(line, " ", trim: true),
         {pid, ""} <- Integer.parse(pid) do
      {:ok, %{pid: pid, start_time: start_time}}
    else
      _ -> :gone
    end
  end

  defp launch_count(path) do
    case File.read(path) do
      {:ok, contents} -> contents |> String.split("\n", trim: true) |> length()
      _ -> 0
    end
  end

  defp assert_eventually(fun, attempts \\ 40)
  defp assert_eventually(_fun, 0), do: flunk("condition was never satisfied")

  defp assert_eventually(fun, attempts) do
    if fun.() do
      :ok
    else
      Process.sleep(25)
      assert_eventually(fun, attempts - 1)
    end
  end
end
