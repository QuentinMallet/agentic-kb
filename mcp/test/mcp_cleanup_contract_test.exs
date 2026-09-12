defmodule AgenticKbMcp.McpCleanupContractTest do
  use ExUnit.Case, async: false

  alias AgenticKbMcp.McpServer

  @no_db_text "No agent-kb.db found. Run `kb init` or `/project-init` to initialise the knowledge base for this project."
  @rebuild_text "Rebuild started in background. Reads continue normally; writes queue until complete."
  @tool_schema_snapshot Path.join(__DIR__, "tool_schema_snapshot.json")
                        |> File.read!()

  # Literal public schema field sets and tool order. Do not generate this from
  # McpServer: it is the characterization fixture for the later registry move.
  @schema_fields [
    {"kb_search",
     ["expand_ids", "inline_verify_k", "limit", "mode", "path_prefix", "query", "tag"]},
    {"kb_add",
     [
       "content",
       "cues",
       "evidence",
       "kind",
       "path",
       "permanent",
       "replace_path",
       "summary",
       "tags"
     ]},
    {"kb_cite", ["end", "path", "start"]},
    {"kb_import", ["path", "upsert"]},
    {"kb_stale_check", ["blame", "commits", "files"]},
    {"kb_expire", ["entry_id", "force", "reason"]},
    {"kb_run", ["adapter", "detail", "result", "test_id"]},
    {"kb_test_add", ["app", "config", "name", "protocol", "test_id"]},
    {"kb_tests", ["app"]},
    {"kb_reembed", ["dry_run", "max_chars"]},
    {"kb_compact", []},
    {"kb_rebuild", []},
    {"kb_audit_run", ["mode", "sample_size"]},
    {"kb_audit_record", ["run_id", "verdicts"]},
    {"kb_audit_report", []},
    {"kb_provenance", ["entry_id", "max_depth"]},
    {"kb_get", ["entry_id"]}
  ]

  # Each expected port map is literal, including intended omission of optional
  # fields. The server generates IDs, so assertions remove only that field.
  @port_requests [
    {"kb_search", %{"query" => "q"}, %{"method" => "search", "query" => "q"}},
    {"kb_add", %{"path" => "a/b", "summary" => "s", "content" => "c"},
     %{"method" => "add", "path" => "a/b", "summary" => "s", "content" => "c"}},
    {"kb_cite", %{"path" => "f", "start" => 1, "end" => 2},
     %{"method" => "cite", "path" => "f", "start" => 1, "end" => 2}},
    {"kb_import", %{"path" => "entries.jsonl"},
     %{"method" => "import", "path" => "entries.jsonl"}},
    {"kb_stale_check", %{"files" => ["f"]}, %{"method" => "stale_check", "files" => ["f"]}},
    {"kb_expire", %{"entry_id" => "e"}, %{"method" => "expire", "entry_id" => "e"}},
    {"kb_run", %{"test_id" => "t", "result" => "pass"},
     %{"method" => "run", "test_id" => "t", "result" => "pass"}},
    {"kb_test_add", %{"app" => "a", "name" => "n", "protocol" => "shell", "config" => "{}"},
     %{
       "method" => "test_add",
       "app" => "a",
       "name" => "n",
       "protocol" => "shell",
       "config" => "{}"
     }},
    {"kb_tests", %{}, %{"method" => "tests"}},
    {"kb_reembed", %{"dry_run" => true}, %{"method" => "reembed", "dry_run" => true}},
    {"kb_compact", %{}, %{"method" => "compact"}},
    {"kb_audit_run", %{"sample_size" => 1, "mode" => "uniform"},
     %{"method" => "audit_run", "sample_size" => 1, "mode" => "uniform"}},
    {"kb_audit_record", %{"run_id" => "run-1", "verdicts" => []},
     %{"method" => "audit_record", "run_id" => "run-1", "verdicts" => []}},
    {"kb_audit_report", %{}, %{"method" => "audit_report"}},
    {"kb_provenance", %{"entry_id" => "e"}, %{"method" => "provenance", "entry_id" => "e"}},
    {"kb_get", %{"entry_id" => "e"}, %{"method" => "kb_get", "entry_id" => "e"}}
  ]

  setup do
    tmp = Path.join(System.tmp_dir!(), "mcp-cleanup-#{System.unique_integer([:positive])}")
    File.mkdir_p!(tmp)
    capture = Path.join(tmp, "requests.jsonl")
    db_path = Path.join([tmp, "agent-kb", "agent-kb.db"])
    File.mkdir_p!(Path.dirname(db_path))
    previous_capture = System.get_env("CAPTURE_FILE")
    System.put_env("CAPTURE_FILE", capture)

    on_exit(fn ->
      if previous_capture,
        do: System.put_env("CAPTURE_FILE", previous_capture),
        else: System.delete_env("CAPTURE_FILE")

      File.rm_rf(tmp)
    end)

    fake = Path.expand("support/capturing_fake_port.sh", __DIR__)
    start_supervised!({AgenticKbMcp.PortManager, db_path: db_path, kb_bin: fake})

    %{capture: capture, db_path: db_path}
  end

  test "tools/list preserves the literal ordered public schema field contract" do
    assert Enum.map(McpServer.tools(), fn tool ->
             {tool["name"], tool["inputSchema"]["properties"] |> Map.keys() |> Enum.sort()}
           end) == @schema_fields
  end

  test "tools/list matches the literal full schema snapshot" do
    assert McpServer.tools() |> :json.encode() |> IO.iodata_to_binary() |> :json.decode() ==
             :json.decode(@tool_schema_snapshot)
  end

  test "public tools/call requests retain literal port maps and omit optional fields", %{
    capture: capture,
    db_path: db_path
  } do
    for {tool, arguments, _expected} <- @port_requests do
      response = call_tool(tool, arguments, %{db_path: db_path})
      refute get_in(response, ["result", "isError"]), "#{tool}: #{inspect(response)}"
    end

    rebuild = call_tool("kb_rebuild", %{}, %{db_path: db_path})
    assert get_in(rebuild, ["result", "content", Access.at(0), "text"]) == @rebuild_text

    captured =
      capture
      |> File.read!()
      |> String.split("\n", trim: true)
      |> Enum.map(&:json.decode/1)
      |> Enum.map(&Map.delete(&1, "id"))

    assert captured == Enum.map(@port_requests, &elem(&1, 2))
    refute Enum.any?(captured, &Map.has_key?(&1, "caller_id"))
  end

  test "no database takes precedence over tool validation and rebuild output" do
    response = call_tool("kb_rebuild", %{"caller_id" => "retired"}, %{db_path: nil})
    assert get_in(response, ["result", "content", Access.at(0), "text"]) == @no_db_text

    response =
      call_tool("kb_search", %{"query" => "q", "caller_id" => "retired"}, %{db_path: nil})

    assert get_in(response, ["result", "content", Access.at(0), "text"]) == @no_db_text
  end

  test "renderer and unknown-tool errors retain exact public text" do
    assert McpServer.render_result(%{"type" => "error", "message" => "port failed"}) == %{
             "content" => [%{"type" => "text", "text" => "port failed"}],
             "isError" => true
           }

    response = call_tool("kb_missing", %{}, %{db_path: "/existing/agent-kb.db"})

    assert get_in(response, ["result", "content", Access.at(0), "text"]) ==
             "Unknown tool: kb_missing"
  end

  defp call_tool(name, arguments, state) do
    line =
      %{
        "jsonrpc" => "2.0",
        "id" => "characterization",
        "method" => "tools/call",
        "params" => %{"name" => name, "arguments" => arguments}
      }
      |> :json.encode()
      |> IO.iodata_to_binary()

    ExUnit.CaptureIO.capture_io(fn ->
      assert {:noreply, _state} = McpServer.handle_cast({:line, line}, state)
    end)
    |> String.trim()
    |> :json.decode()
  end
end
