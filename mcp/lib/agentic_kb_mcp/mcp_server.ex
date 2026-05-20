defmodule AgenticKbMcp.McpServer do
  @moduledoc """
  MCP JSON-RPC 2.0 stdio handler. Reads requests from stdin line-by-line,
  dispatches to PortManager (or returns no-db errors), writes responses to stdout.
  """

  use GenServer
  require Logger

  @protocol_version "2024-11-05"
  @server_info %{"name" => "agentic-kb-mcp", "version" => "0.1.0"}

  @tools [
    %{
      "name" => "kb_search",
      "description" => "Search the agent knowledge base (FTS + semantic hybrid)",
      "inputSchema" => %{
        "type" => "object",
        "properties" => %{
          "query" => %{"type" => "string", "description" => "Search query"},
          "limit" => %{"type" => "integer", "description" => "Max results (default 10)"},
          "mode" => %{
            "type" => "string",
            "enum" => ["hybrid", "fts", "semantic"],
            "description" => "Search mode (default: hybrid)"
          }
        },
        "required" => ["query"]
      }
    },
    %{
      "name" => "kb_add",
      "description" => "Add or update a knowledge entry in the agent knowledge base",
      "inputSchema" => %{
        "type" => "object",
        "properties" => %{
          "path" => %{
            "type" => "string",
            "description" => "Category/topic path (e.g. elixir/ash-framework)"
          },
          "summary" => %{"type" => "string", "description" => "One-line summary"},
          "content" => %{"type" => "string", "description" => "Full markdown content"},
          "tags" => %{
            "type" => "array",
            "items" => %{"type" => "string"},
            "description" => "Topic tags"
          }
        },
        "required" => ["path", "summary", "content"]
      }
    },
    %{
      "name" => "kb_import",
      "description" => "Bulk-import entries from a seed JSON file",
      "inputSchema" => %{
        "type" => "object",
        "properties" => %{
          "path" => %{
            "type" => "string",
            "description" => "Absolute path to a seed JSON file (array of entries)"
          },
          "upsert" => %{
            "type" => "boolean",
            "description" => "Overwrite existing entries (default false)"
          }
        },
        "required" => ["path"]
      }
    },
    %{
      "name" => "kb_rebuild",
      "description" => "Rebuild the embedding index by replaying all events",
      "inputSchema" => %{
        "type" => "object",
        "properties" => %{}
      }
    }
  ]

  # ---------------------------------------------------------------------------
  # Public API
  # ---------------------------------------------------------------------------

  def start_link(opts) do
    GenServer.start_link(__MODULE__, opts, name: __MODULE__)
  end

  # ---------------------------------------------------------------------------
  # GenServer callbacks
  # ---------------------------------------------------------------------------

  @impl true
  def init(opts) do
    db_path = Keyword.get(opts, :db_path)
    parent = self()
    Task.start_link(fn -> read_stdin(parent) end)
    {:ok, %{db_path: db_path}}
  end

  @impl true
  def handle_cast(:eof, _state) do
    System.halt(0)
  end

  def handle_cast({:error, reason}, _state) do
    Logger.error("stdin error: #{inspect(reason)}")
    System.halt(1)
  end

  def handle_cast({:line, ""}, state), do: {:noreply, state}

  def handle_cast({:line, line}, state) do
    case json_decode(line) do
      {:ok, request} ->
        case handle_request(request, state) do
          nil -> :ok
          response -> write_response(response)
        end

      {:error, _} ->
        write_response(%{
          "jsonrpc" => "2.0",
          "id" => nil,
          "error" => %{"code" => -32_700, "message" => "Parse error"}
        })
    end

    {:noreply, state}
  end

  # ---------------------------------------------------------------------------
  # MCP method handlers
  # ---------------------------------------------------------------------------

  defp handle_request(%{"method" => "initialize", "id" => id}, _state) do
    %{
      "jsonrpc" => "2.0",
      "id" => id,
      "result" => %{
        "protocolVersion" => @protocol_version,
        "serverInfo" => @server_info,
        "capabilities" => %{"tools" => %{}}
      }
    }
  end

  defp handle_request(%{"method" => "initialized"}, _state), do: nil

  defp handle_request(%{"method" => "tools/list", "id" => id}, _state) do
    %{"jsonrpc" => "2.0", "id" => id, "result" => %{"tools" => @tools}}
  end

  defp handle_request(
         %{"method" => "tools/call", "id" => id, "params" => %{"name" => tool} = params},
         state
       ) do
    args = Map.get(params, "arguments", %{})
    result = dispatch_tool(tool, args, state)
    %{"jsonrpc" => "2.0", "id" => id, "result" => result}
  end

  defp handle_request(%{"method" => "notifications/" <> _, "id" => _id}, _state), do: nil
  defp handle_request(%{"method" => "notifications/" <> _}, _state), do: nil

  defp handle_request(%{"method" => method, "id" => id}, _state) do
    %{
      "jsonrpc" => "2.0",
      "id" => id,
      "error" => %{"code" => -32_601, "message" => "Method not found: #{method}"}
    }
  end

  defp handle_request(_req, _state), do: nil

  # ---------------------------------------------------------------------------
  # Tool dispatch
  # ---------------------------------------------------------------------------

  defp dispatch_tool(_tool, _args, %{db_path: nil}) do
    text_error(
      "No agent-kb.db found. Run `kb init` or `/project-init` to initialise the knowledge base for this project."
    )
  end

  defp dispatch_tool("kb_search", args, _state) do
    req =
      %{"method" => "search", "id" => gen_id()}
      |> put_if_present("query", args["query"])
      |> put_if_present("limit", args["limit"])
      |> put_if_present("mode", args["mode"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_add", args, _state) do
    req =
      %{"method" => "add", "id" => gen_id()}
      |> put_if_present("path", args["path"])
      |> put_if_present("summary", args["summary"])
      |> put_if_present("content", args["content"])
      |> put_if_present("tags", args["tags"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_import", args, _state) do
    req =
      %{"method" => "import", "id" => gen_id()}
      |> put_if_present("path", args["path"])
      |> put_if_present("upsert", args["upsert"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_rebuild", _args, _state) do
    req = %{"method" => "rebuild", "id" => gen_id()}
    port_call_to_content(req)
  end

  defp dispatch_tool(name, _args, _state) do
    text_error("Unknown tool: #{name}")
  end

  # ---------------------------------------------------------------------------
  # Port call helpers
  # ---------------------------------------------------------------------------

  defp port_call_to_content(req) do
    case AgenticKbMcp.PortManager.call_port(req) do
      %{"type" => "result", "entries" => entries} ->
        %{"content" => [%{"type" => "text", "text" => format_entries(entries)}]}

      %{"type" => "ok", "imported" => imported, "skipped" => skipped} ->
        %{
          "content" => [
            %{"type" => "text", "text" => "Imported #{imported} entries (#{skipped} skipped)."}
          ]
        }

      %{"type" => "ok", "rebuilt" => rebuilt} ->
        %{"content" => [%{"type" => "text", "text" => "Rebuilt #{rebuilt} entries."}]}

      %{"type" => "ok", "entry_id" => entry_id} ->
        %{"content" => [%{"type" => "text", "text" => "Added entry #{entry_id}."}]}

      %{"type" => "ok"} ->
        %{"content" => [%{"type" => "text", "text" => "OK"}]}

      %{"type" => "error", "message" => msg} ->
        text_error(msg)

      other ->
        %{"content" => [%{"type" => "text", "text" => json_encode!(other)}]}
    end
  end

  defp format_entries([]), do: "(no results)"

  defp format_entries(entries) do
    entries
    |> Enum.map(fn e ->
      path = e["path"] || ""
      summary = e["summary"] || ""
      content = e["content"] || ""
      score = e["score"]
      score_str = if score, do: " (score: #{Float.round(score * 1.0, 3)})", else: ""
      "## #{path}#{score_str}\n#{summary}\n\n#{content}"
    end)
    |> Enum.join("\n\n---\n\n")
  end

  defp text_error(msg) do
    %{"content" => [%{"type" => "text", "text" => msg}], "isError" => true}
  end

  # ---------------------------------------------------------------------------
  # Stdin reader (runs in a Task)
  # ---------------------------------------------------------------------------

  defp read_stdin(server) do
    case IO.read(:stdio, :line) do
      :eof ->
        GenServer.cast(server, :eof)

      {:error, reason} ->
        GenServer.cast(server, {:error, reason})

      line ->
        GenServer.cast(server, {:line, String.trim(line)})
        read_stdin(server)
    end
  end

  # ---------------------------------------------------------------------------
  # Utility
  # ---------------------------------------------------------------------------

  defp write_response(response) do
    IO.puts(json_encode!(response))
  end

  defp put_if_present(map, _key, nil), do: map
  defp put_if_present(map, key, value), do: Map.put(map, key, value)

  defp gen_id do
    :crypto.strong_rand_bytes(8) |> Base.encode16(case: :lower)
  end

  defp json_decode(binary) do
    try do
      {:ok, :json.decode(binary)}
    catch
      _, _ -> {:error, :invalid_json}
    end
  end

  defp json_encode!(term) do
    term |> :json.encode() |> IO.iodata_to_binary()
  end
end
