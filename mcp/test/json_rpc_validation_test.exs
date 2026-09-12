defmodule AgenticKbMcp.JsonRpcTest do
  use ExUnit.Case, async: true

  alias AgenticKbMcp.JsonRpc
  alias AgenticKbMcp.McpServer

  defp call_line(line) do
    output =
      ExUnit.CaptureIO.capture_io(fn ->
        assert {:noreply, _state} = McpServer.handle_cast({:line, line}, %{db_path: nil})
      end)

    if output == "", do: nil, else: output |> String.trim() |> :json.decode()
  end

  test "rejects malformed decoded requests with an invalid-request response" do
    for request <- [
          %{},
          %{"jsonrpc" => "1.0", "method" => "initialize", "id" => 1},
          %{"jsonrpc" => "2.0", "method" => 1, "id" => 1},
          %{"jsonrpc" => "2.0", "method" => "initialize", "id" => nil},
          %{"jsonrpc" => "2.0", "method" => "initialize", "id" => 1, "params" => []}
        ] do
      assert {:error, response} = JsonRpc.classify(request)
      assert %{"id" => :null, "error" => %{"code" => -32600}} = response
    end
  end

  test "recognizes a valid request and preserves a legal MCP id" do
    assert {:request, "initialize", 1, %{}} =
             JsonRpc.classify(%{"jsonrpc" => "2.0", "method" => "initialize", "id" => 1})

    assert {:request, "tools/list", "a", %{}} =
             JsonRpc.classify(%{"jsonrpc" => "2.0", "method" => "tools/list", "id" => "a"})
  end

  test "valid notifications are response-free" do
    assert {:notification, "initialized", %{}} =
             JsonRpc.classify(%{"jsonrpc" => "2.0", "method" => "initialized"})

    assert {:notification, "tools/call", %{"name" => "kb_search"}} =
             JsonRpc.classify(%{
               "jsonrpc" => "2.0",
               "method" => "tools/call",
               "params" => %{"name" => "kb_search"}
             })
  end

  test "invalid tools/call parameters are invalid params only for requests" do
    request = %{"jsonrpc" => "2.0", "method" => "tools/call", "id" => 1, "params" => %{}}
    assert {:error, response} = JsonRpc.validate_method(request)
    assert %{"id" => 1, "error" => %{"code" => -32602}} = response

    notification = Map.delete(request, "id")
    assert {:notification, "tools/call", %{}} = JsonRpc.classify(notification)
  end

  test "invalid decoded requests respond and never dispatch" do
    response = call_line(~s({"jsonrpc":"2.0","method":"tools/call","id":1,"params":{}}))
    assert %{"id" => 1, "error" => %{"code" => -32602}} = response

    response = call_line(~s({"jsonrpc":"2.0","method":1}))
    assert %{"id" => :null, "error" => %{"code" => -32600}} = response
  end

  test "valid no-id tools/call is silent and does not dispatch" do
    assert nil ==
             call_line(
               ~s({"jsonrpc":"2.0","method":"tools/call","params":{"name":"kb_search","arguments":{"query":"x"}}})
             )
  end

  test "initialize advertises the application manifest version" do
    response = call_line(~s({"jsonrpc":"2.0","method":"initialize","id":1,"params":{}}))
    expected = Application.spec(:agentic_kb_mcp, :vsn) |> to_string()
    assert get_in(response, ["result", "serverInfo", "version"]) == expected
  end
end
