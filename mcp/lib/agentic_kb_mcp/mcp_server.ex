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
          },
          "path_prefix" => %{
            "type" => "string",
            "description" => "Filter results to entries whose path starts with this prefix"
          },
          "tag" => %{
            "type" => "string",
            "description" => "Filter results to entries that have this exact tag"
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
          },
          "permanent" => %{
            "type" => "boolean",
            "description" => "Mark entry as permanent (survives compact and resists expire)"
          },
          "replace_path" => %{
            "type" => "boolean",
            "description" => "Expire all existing entries at this path before inserting"
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
      "name" => "kb_stale_check",
      "description" => "Check if KB entries for given files are stale (file changed since entry was recorded)",
      "inputSchema" => %{
        "type" => "object",
        "properties" => %{
          "files" => %{
            "type" => "array",
            "items" => %{"type" => "string"},
            "description" => "File paths to check for stale KB entries"
          }
        },
        "required" => ["files"]
      }
    },
    %{
      "name" => "kb_expire",
      "description" => "Mark an entry as stale (expired). Permanent entries require force=true.",
      "inputSchema" => %{
        "type" => "object",
        "properties" => %{
          "entry_id" => %{"type" => "string", "description" => "Entry ID to expire"},
          "reason" => %{"type" => "string", "description" => "Reason for expiration"},
          "force" => %{
            "type" => "boolean",
            "description" => "Force expiration of permanent entries (default false)"
          }
        },
        "required" => ["entry_id"]
      }
    },
    %{
      "name" => "kb_reembed",
      "description" => "Re-embed entries missing embeddings (e.g. written with KB_NO_EMBED=1)",
      "inputSchema" => %{
        "type" => "object",
        "properties" => %{
          "dry_run" => %{
            "type" => "boolean",
            "description" => "Show what would be re-embedded without writing (default false)"
          },
          "max_chars" => %{
            "type" => "integer",
            "description" => "Skip entries exceeding this char limit (default 1800)"
          }
        }
      }
    },
    %{
      "name" => "kb_compact",
      "description" => "Compact the event log by squashing superseded events",
      "inputSchema" => %{
        "type" => "object",
        "properties" => %{}
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
      |> put_if_present("path_prefix", args["path_prefix"])
      |> put_if_present("tag", args["tag"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_add", args, _state) do
    req =
      %{"method" => "add", "id" => gen_id()}
      |> put_if_present("path", args["path"])
      |> put_if_present("summary", args["summary"])
      |> put_if_present("content", args["content"])
      |> put_if_present("tags", args["tags"])
      |> put_if_present("permanent", args["permanent"])
      |> put_if_present("replace_path", args["replace_path"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_import", args, _state) do
    req =
      %{"method" => "import", "id" => gen_id()}
      |> put_if_present("path", args["path"])
      |> put_if_present("upsert", args["upsert"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_stale_check", args, _state) do
    req =
      %{"method" => "stale_check", "id" => gen_id()}
      |> put_if_present("files", args["files"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_expire", args, _state) do
    req =
      %{"method" => "expire", "id" => gen_id()}
      |> put_if_present("entry_id", args["entry_id"])
      |> put_if_present("reason", args["reason"])
      |> put_if_present("force", args["force"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_reembed", args, _state) do
    req =
      %{"method" => "reembed", "id" => gen_id()}
      |> put_if_present("dry_run", args["dry_run"])
      |> put_if_present("max_chars", args["max_chars"])

    port_call_to_content(req)
  end

  defp dispatch_tool("kb_compact", _args, _state) do
    req = %{"method" => "compact", "id" => gen_id()}
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

      %{"type" => "ok", "embedded" => embedded} = resp ->
        parts = ["Re-embedded #{embedded} entries."]
        parts = if resp["failed"], do: parts ++ ["#{resp["failed"]} failed."], else: parts
        parts = if resp["skipped"], do: parts ++ ["#{resp["skipped"]} skipped (too large)."], else: parts
        parts = if resp["missing"], do: parts ++ ["#{resp["missing"]} missing embeddings."], else: parts
        parts = if resp["dry_run"], do: ["[dry-run] " | parts], else: parts
        parts = if resp["message"], do: parts ++ [resp["message"]], else: parts
        %{"content" => [%{"type" => "text", "text" => Enum.join(parts, " ")}]}

      %{"type" => "ok", "before" => before, "after" => after_count} ->
        %{
          "content" => [
            %{"type" => "text", "text" => "Compacted: #{before} events -> #{after_count}."}
          ]
        }

      %{"type" => "ok", "rebuilt" => rebuilt} ->
        %{"content" => [%{"type" => "text", "text" => "Rebuilt #{rebuilt} entries."}]}

      %{"type" => "ok", "entry_id" => entry_id} ->
        %{"content" => [%{"type" => "text", "text" => "Added entry #{entry_id}."}]}

      %{"type" => "ok", "expired" => expired_id} ->
        %{"content" => [%{"type" => "text", "text" => "Expired entry #{expired_id}."}]}

      %{"type" => "result", "stale" => stale, "checked" => checked} ->
        text =
          if stale == [] do
            "Checked #{checked} file(s): all KB entries are up to date."
          else
            header = "Found #{length(stale)} stale entry/entries (#{checked} file(s) checked):\n\n"

            details =
              Enum.map_join(stale, "\n", fn e ->
                "STALE [#{e["path"]}] #{e["summary"]}  id=#{e["id"]}  recorded-at=#{e["version_ref"]}  (#{e["commits_behind"]} commit(s) ago)"
              end)

            header <> details
          end

        %{"content" => [%{"type" => "text", "text" => text}]}

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
