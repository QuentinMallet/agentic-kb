defmodule AgenticKbMcp.OptionBAccessTest do
  use ExUnit.Case, async: false

  alias AgenticKbMcp.{McpServer, RebuildManager}

  # The MCP registry is finite.  This table deliberately drives every public
  # tool through the real JSON-RPC -> tools/call -> port dispatch path with no
  # Authorization process in state.  A table is more useful than a generated
  # property here: each entry supplies the smallest valid request fixture for
  # a distinct public operation.  The project has no Elixir property-testing
  # dependency, and this test adds none.
  @tool_arguments %{
    "kb_search" => %{"query" => "option-b"},
    "kb_add" => %{"path" => "option-b/access", "summary" => "s", "content" => "c"},
    "kb_cite" => %{"path" => "fixture.txt", "start" => 1, "end" => 1},
    "kb_import" => %{"path" => "fixture.jsonl"},
    "kb_stale_check" => %{"files" => ["fixture.txt"]},
    "kb_expire" => %{"entry_id" => "entry-1"},
    "kb_run" => %{"test_id" => "test-1", "result" => "pass"},
    "kb_test_add" => %{"app" => "app", "name" => "name", "protocol" => "shell", "config" => "{}"},
    "kb_tests" => %{},
    "kb_reembed" => %{"dry_run" => true},
    "kb_compact" => %{},
    "kb_rebuild" => %{},
    "kb_audit_run" => %{"sample_size" => 1, "mode" => "uniform"},
    "kb_audit_record" => %{"run_id" => "audit-1", "verdicts" => []},
    "kb_audit_report" => %{},
    "kb_provenance" => %{"entry_id" => "entry-1"},
    "kb_get" => %{"entry_id" => "entry-1"}
  }

  setup do
    fake = Path.expand("support/fake_port.sh", __DIR__)

    start_supervised!(
      {AgenticKbMcp.PortManager, db_path: "unused", kb_bin: fake, name: AgenticKbMcp.PortManager}
    )

    rebuild_manager_name = :"option_b_rebuild_#{System.unique_integer([:positive, :monotonic])}"

    start_supervised!(
      {RebuildManager, db_path: "unused", kb_bin: fake, name: rebuild_manager_name}
    )

    %{rebuild_manager_name: rebuild_manager_name}
  end

  defp call_tool(name, arguments, rebuild_manager_name) do
    request = %{
      "jsonrpc" => "2.0",
      "id" => 1,
      "method" => "tools/call",
      "params" => %{"name" => name, "arguments" => arguments}
    }

    output =
      ExUnit.CaptureIO.capture_io(fn ->
        assert {:noreply, _state} =
                 McpServer.handle_cast(
                   {:line, request |> :json.encode() |> IO.iodata_to_binary()},
                   %{
                     db_path: "/unused/agent-kb.db",
                     rebuild_manager_name: rebuild_manager_name
                   }
                 )
      end)

    output |> String.trim() |> :json.decode()
  end

  test "every advertised tool is available without package authorization", %{
    rebuild_manager_name: manager
  } do
    registered = McpServer.tools() |> Enum.map(& &1["name"]) |> MapSet.new()
    assert registered == MapSet.new(Map.keys(@tool_arguments))

    for {name, arguments} <- @tool_arguments do
      response = call_tool(name, arguments, manager)

      assert %{"result" => result} = response,
             "#{name} must produce a tools/call result without package authorization: #{inspect(response)}"

      refute result["isError"],
             "#{name} must not be rejected by a package permission gate: #{inspect(result)}"
    end
  end

  test "formerly guarded expire and audit operations dispatch anonymously", %{
    rebuild_manager_name: manager
  } do
    for {name, arguments} <-
          Map.take(@tool_arguments, ["kb_expire", "kb_audit_run", "kb_audit_record"]) do
      response = call_tool(name, arguments, manager)

      assert %{"result" => result} = response

      refute result["isError"],
             "#{name} must not require an Authorization process: #{inspect(result)}"
    end
  end

  test "client caller_id is rejected by every closed tool schema", %{
    rebuild_manager_name: manager
  } do
    for {name, arguments} <- @tool_arguments do
      response = call_tool(name, Map.put(arguments, "caller_id", "client-supplied"), manager)

      assert %{"result" => %{"isError" => true, "content" => [%{"text" => text}]}} = response
      assert text =~ "caller_id"
      assert text =~ "unknown argument"
    end
  end
end
