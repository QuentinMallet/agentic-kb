defmodule AgenticKbMcp.ApplicationLifecycleTest do
  use ExUnit.Case, async: false

  alias AgenticKbMcp.{McpServer, PortManager}

  @fake_port Path.expand("support/fake_port.sh", __DIR__)
  @silent_crash Path.expand("support/fake_port_silent_crash.sh", __DIR__)

  setup do
    File.chmod!(@fake_port, 0o755)
    File.chmod!(@silent_crash, 0o755)

    stop_application()
    {:ok, _} = Elixir.Application.ensure_all_started(:crypto)
    {:ok, _} = Elixir.Application.ensure_all_started(:logger)
    previous_opts = Elixir.Application.get_env(:agentic_kb_mcp, :startup_opts)

    db_path =
      Path.join(System.tmp_dir!(), "mcp-lifecycle-#{System.unique_integer([:positive])}.db")

    File.write!(db_path, "")

    on_exit(fn ->
      stop_application()
      restore_startup_opts(previous_opts)
      File.rm(db_path)
    end)

    {:ok, db_path: db_path}
  end

  test "production child specs start once, survive a PortManager restart, and stop cleanly", %{
    db_path: db_path
  } do
    configure_startup(db_path, @fake_port)

    assert :ok = Elixir.Application.start(:agentic_kb_mcp)
    supervisor = Process.whereis(AgenticKbMcp.Supervisor)
    assert is_pid(supervisor)

    %{port_manager: port_manager, server: server, input_port: input_port} = lifecycle_pids(supervisor)
    assert Process.alive?(port_manager)
    assert Process.alive?(server)
    assert is_port(input_port)
    assert Port.info(input_port)

    Process.exit(port_manager, :kill)

    assert_eventually(fn ->
      case lifecycle_pids(supervisor) do
        %{port_manager: restarted_port_manager, server: ^server, input_port: ^input_port} ->
          restarted_port_manager != port_manager and Process.alive?(restarted_port_manager) and
            Port.info(input_port)

        _ ->
          false
      end
    end)

    Process.exit(server, :kill)

    %{server: restarted_server, input_port: restarted_input_port} =
      assert_eventually_value(fn ->
        case lifecycle_pids(supervisor) do
          %{server: next_server, input_port: next_input_port} ->
            if next_server != server and next_input_port != input_port and Port.info(next_input_port) do
              %{server: next_server, input_port: next_input_port}
            end

          _ ->
            nil
        end
      end)

    refute Port.info(input_port)
    assert Process.alive?(restarted_server)
    assert Port.info(restarted_input_port)

    assert :ok = Elixir.Application.stop(:agentic_kb_mcp)
    refute Process.alive?(restarted_server)
    refute Port.info(restarted_input_port)
  end

  test "a PortManager child startup crash fails the application start without a live supervisor",
       %{
         db_path: db_path
       } do
    configure_startup(db_path, @silent_crash)

    assert {:error, _reason} = Elixir.Application.start(:agentic_kb_mcp)
    refute Process.whereis(AgenticKbMcp.Supervisor)
  end

  defp configure_startup(db_path, kb_bin) do
    # This is deliberately configuration of the same child-spec builder the
    # production Application callback uses. The held-open input port prevents
    # test runner stdin from becoming a false lifecycle result.
    Elixir.Application.put_env(:agentic_kb_mcp, :startup_opts,
      db_path: db_path,
      kb_bin: kb_bin,
      port_manager_name: PortManager,
      input_port_factory: &AgenticKbMcp.TestSupport.HeldInput.open/0
    )
  end

  defp lifecycle_pids(supervisor) do
    children = Supervisor.which_children(supervisor)

    with port_manager when is_pid(port_manager) <- child_pid(children, PortManager),
         server when is_pid(server) <- child_pid(children, McpServer),
         {:ok, %{port: input_port}} <- server_state(server),
         true <- is_port(input_port) do
      %{port_manager: port_manager, server: server, input_port: input_port}
    else
      _ -> nil
    end
  end

  defp server_state(server) do
    {:ok, :sys.get_state(server)}
  catch
    :exit, _reason -> :gone
  end

  defp child_pid(children, module) do
    case Enum.find(children, fn {_id, _pid, _type, modules} -> module in List.wrap(modules) end) do
      {_id, pid, _type, _modules} when is_pid(pid) -> pid
      _ -> nil
    end
  end

  defp assert_eventually(fun, attempts \\ 40)
  defp assert_eventually(fun, 0), do: assert(fun.())

  defp assert_eventually(fun, attempts) do
    if fun.() do
      :ok
    else
      Process.sleep(25)
      assert_eventually(fun, attempts - 1)
    end
  end

  defp assert_eventually_value(fun, attempts \\ 40)
  defp assert_eventually_value(_fun, 0), do: flunk("condition was never satisfied")

  defp assert_eventually_value(fun, attempts) do
    case fun.() do
      nil ->
        Process.sleep(25)
        assert_eventually_value(fun, attempts - 1)

      value ->
        value
    end
  end

  defp stop_application do
    case Elixir.Application.stop(:agentic_kb_mcp) do
      :ok -> :ok
      {:error, {:not_started, :agentic_kb_mcp}} -> :ok
    end
  end

  defp restore_startup_opts(nil),
    do: Elixir.Application.delete_env(:agentic_kb_mcp, :startup_opts)

  defp restore_startup_opts(opts),
    do: Elixir.Application.put_env(:agentic_kb_mcp, :startup_opts, opts)
end
