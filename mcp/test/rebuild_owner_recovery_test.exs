defmodule AgenticKbMcp.RebuildOwnerRecoveryTest do
  use ExUnit.Case, async: false

  alias AgenticKbMcp.RebuildManager

  @kb Path.expand("../../target/debug/kb", __DIR__)

  setup do
    previous_no_embed = System.get_env("KB_NO_EMBED")
    System.put_env("KB_NO_EMBED", "1")
    root = Path.join(System.tmp_dir!(), "mcp-owner-#{System.unique_integer([:positive])}")
    File.mkdir_p!(root)
    File.mkdir_p!(Path.join(root, ".state"))

    {_, 0} =
      System.cmd(
        @kb,
        [
          "add",
          "--path",
          "fixture/owner",
          "--summary",
          "owner",
          "--content",
          "owner",
          "--tags",
          "fixture"
        ],
        cd: root
      )

    db = Path.join([root, ".state", "agent-kb", "agent-kb.db"])

    on_exit(fn ->
      if previous_no_embed,
        do: System.put_env("KB_NO_EMBED", previous_no_embed),
        else: System.delete_env("KB_NO_EMBED")

      File.rm_rf!(root)
    end)

    %{root: root, db: db}
  end

  test "owner death releases the real child before replacement rebuild", %{root: root, db: db} do
    supervisor = start_supervised!({DynamicSupervisor, strategy: :one_for_one})
    manager_name = :"rebuild_owner_recovery_#{System.unique_integer([:positive, :monotonic])}"

    assert {:ok, _} =
             DynamicSupervisor.start_child(
               supervisor,
               {RebuildManager, db_path: db, kb_bin: @kb, name: manager_name}
             )

    old_owner = child_pid(supervisor)
    compute = hold_lock(Path.join([root, ".state", ".lock"]))
    assert {:ok, :started} = RebuildManager.request_rebuild(manager_name)
    %{port: old_port} = :sys.get_state(old_owner)
    old_pid = Port.info(old_port)[:os_pid]
    old_start = proc_start_time(old_pid)
    assert old_start
    Process.exit(old_owner, :kill)

    owner =
      eventually(fn ->
        pid = child_pid(supervisor)
        if is_pid(pid) and pid != old_owner and Process.alive?(pid), do: pid
      end)

    eventually(fn -> proc_start_time(old_pid) != old_start end)

    # Rust Path::with_extension on the dotfile `.lock` appends this suffix.
    lifetime = Path.join([root, ".state", ".lock.rebuild-lifetime.lock"])
    blocker = hold_lock(lifetime)
    assert {:error, {:launch_failed, message}} = RebuildManager.request_rebuild(manager_name)
    assert message =~ "acquire supervised rebuild lifetime lock"
    assert {:ok, %{exit_status: _}} = RebuildManager.await_terminal(manager_name)
    blocker_pid = Port.info(blocker)[:os_pid]
    blocker_start = proc_start_time(blocker_pid)
    Port.close(blocker)
    eventually(fn -> proc_start_time(blocker_pid) != blocker_start end)

    assert {:ok, :started} = RebuildManager.request_rebuild(manager_name)
    %{port: port} = :sys.get_state(owner)
    os_pid = Port.info(port)[:os_pid]
    assert is_integer(os_pid)
    assert proc_start_time(os_pid)
    Port.close(compute)
    assert {:ok, %{exit_status: 0}} = RebuildManager.await_terminal(manager_name)
  end

  defp hold_lock(path) do
    File.mkdir_p!(Path.dirname(path))
    python = System.find_executable("python3") || raise "python3 unavailable"

    program =
      "import fcntl,sys; f=open(sys.argv[1],'w'); fcntl.flock(f,fcntl.LOCK_EX); print('ready',flush=True); sys.stdin.read()"

    port =
      Port.open({:spawn_executable, python}, [
        :binary,
        :use_stdio,
        :exit_status,
        args: ["-c", program, path]
      ])

    receive do
      {^port, {:data, "ready\n"}} ->
        on_exit(fn ->
          if Port.info(port), do: Port.close(port)
        end)

        port
    after
      1_000 -> flunk("lock holder did not acquire #{path}")
    end
  end

  defp proc_start_time(pid) do
    case File.read("/proc/#{pid}/stat") do
      {:ok, stat} -> stat |> String.split() |> Enum.at(21)
      {:error, _} -> nil
    end
  end

  defp child_pid(supervisor) do
    Supervisor.which_children(supervisor)
    |> Enum.find_value(fn {_id, pid, _type, modules} ->
      if RebuildManager in List.wrap(modules), do: pid
    end)
  end

  defp eventually(fun, attempts \\ 80)
  defp eventually(_fun, 0), do: flunk("condition not reached")

  defp eventually(fun, attempts),
    do:
      fun.() ||
        (
          Process.sleep(25)
          eventually(fun, attempts - 1)
        )
end
