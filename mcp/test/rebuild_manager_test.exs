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
    db_path = Path.join([root, ".state", "agent-kb", "agent-kb.db"])
    File.mkdir_p!(Path.dirname(db_path))
    File.write!(db_path, "")

    on_exit(fn -> File.rm_rf!(root) end)

    %{root: root, pid_file: pid_file, args_file: args_file, db_path: db_path}
  end

  test "launch is synchronous, coalesces an active child, and binds the active database", ctx do
    {manager, _pid} = start_manager(ctx, :hold)

    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    assert_eventually(fn -> File.exists?(ctx.pid_file) end)
    assert os_pid_alive?(ctx.pid_file)
    assert File.read!(ctx.args_file) =~ "rebuild --db #{ctx.db_path} --supervised"

    assert {:ok, :already_running} = RebuildManager.request_rebuild(manager)
    assert File.read!(ctx.pid_file) |> String.split("\n", trim: true) |> length() == 1
  end

  test "private cancellation waits for observed OS exit before a retry can launch", ctx do
    {manager, _pid} = start_manager(ctx, :hold)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
    assert_eventually(fn -> os_pid_alive?(ctx.pid_file) end)

    assert :ok = RebuildManager.cancel_and_await(manager, 2_000)
    refute os_pid_alive?(ctx.pid_file)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)
  end

  test "a direct launch failure is returned rather than acknowledged as started", ctx do
    {manager, _pid} = start_manager(ctx, :fail)

    assert {:error, {:launch_failed, _reason}} = RebuildManager.request_rebuild(manager)
    refute File.exists?(Path.join(ctx.root, "rebuild.log"))
  end

  test "captured rebuild output is bounded", ctx do
    {manager, _pid} = start_manager(ctx, :flood)
    assert {:ok, :started} = RebuildManager.request_rebuild(manager)

    log = Path.join(ctx.root, "rebuild.log")
    assert_eventually(fn -> File.exists?(log) end)
    assert File.stat!(log).size <= @log_limit
  end

  test "ordinary MCP-port requests remain responsive while rebuild holds its lock", ctx do
    {manager, _pid} = start_manager(ctx, :hold)
    port_name = :"rebuild_port_#{System.unique_integer([:positive, :monotonic])}"
    File.chmod!(@mcp_fixture, 0o755)

    start_supervised!({PortManager, db_path: ctx.db_path, kb_bin: @mcp_fixture, name: port_name})

    assert {:ok, :started} = RebuildManager.request_rebuild(manager)

    assert %{"id" => "ordinary-read", "type" => "result"} =
             PortManager.call_port(%{"id" => "ordinary-read", "method" => "echo"}, 1_000, port_name)
  end

  defp start_manager(ctx, mode) do
    name = :"rebuild_#{System.unique_integer([:positive, :monotonic])}"

    previous = System.get_env("REBUILD_FIXTURE_MODE")
    System.put_env("REBUILD_FIXTURE_MODE", Atom.to_string(mode))
    System.put_env("REBUILD_PID_FILE", ctx.pid_file)
    System.put_env("REBUILD_ARGS_FILE", ctx.args_file)

    on_exit(fn ->
      restore_env("REBUILD_FIXTURE_MODE", previous)
      System.delete_env("REBUILD_PID_FILE")
      System.delete_env("REBUILD_ARGS_FILE")
    end)

    {name, start_supervised!({RebuildManager, db_path: ctx.db_path, kb_bin: @fixture, name: name})}
  end

  defp restore_env(key, nil), do: System.delete_env(key)
  defp restore_env(key, value), do: System.put_env(key, value)

  defp os_pid_alive?(pid_file) do
    with {:ok, contents} <- File.read(pid_file),
         {pid, ""} <- Integer.parse(String.trim(contents)) do
      System.cmd("kill", ["-0", Integer.to_string(pid)], stderr_to_stdout: true)
      |> elem(1)
      |> Kernel.==(0)
    else
      _ -> false
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
