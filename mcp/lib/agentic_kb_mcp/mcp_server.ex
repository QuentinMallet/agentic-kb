defmodule AgenticKbMcp.McpServer do
  @moduledoc """
  MCP JSON-RPC 2.0 stdio handler. Reads requests from stdin line-by-line,
  dispatches to PortManager (or returns no-db errors), writes responses to stdout.
  """

  use GenServer
  require Logger

  alias AgenticKbMcp.JsonRpc
  alias AgenticKbMcp.Transport
  alias AgenticKbMcp.Transport.Stdio

  @protocol_version "2024-11-05"
  @server_name "agentic-kb-mcp"
  @format_entries_max_bytes 32_000
  @evidence_preview_limit 3

  @doc "Exposes the tool schema list for testing (tools/list mirrors this)."
  def tools, do: AgenticKbMcp.ToolRegistry.tools()

  @doc false
  def validate_tool_args(tool, args), do: AgenticKbMcp.ToolRegistry.validate_tool_args(tool, args)

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
    input_port_factory = Keyword.get(opts, :input_port_factory, &open_stdin_port/0)
    port = input_port_factory.()

    {:ok,
     %{
       db_path: db_path,
       port: port,
       framer: Stdio.new(Transport.max_frame_bytes()),
       stdin_eof: false
     }}
  end

  @impl true
  def terminate(_reason, %{port: port}) do
    if Port.info(port), do: Port.close(port)
  end

  @impl true
  def handle_cast(:eof, _state) do
    System.halt(0)
  end

  def handle_cast({:error, reason}, _state) do
    Logger.error("stdin error: #{inspect(reason)}")
    System.halt(1)
  end

  def handle_cast(:frame_too_large, state) do
    write_response(%{
      "jsonrpc" => "2.0",
      "id" => :null,
      "error" => %{"code" => -32_700, "message" => "Frame exceeds 10 MiB limit"}
    })

    {:noreply, state}
  end

  def handle_cast({:line, ""}, state), do: {:noreply, state}

  def handle_cast({:line, line}, state) do
    case json_decode(line) do
      {:ok, request} ->
        case validated_request(request) do
          nil ->
            :ok

          {:request, request} ->
            case handle_request(request, state) do
              nil -> :ok
              response -> write_response(response)
            end

          response ->
            write_response(response)
        end

      {:error, _} ->
        write_response(%{
          "jsonrpc" => "2.0",
          "id" => :null,
          "error" => %{"code" => -32_700, "message" => "Parse error"}
        })
    end

    {:noreply, state}
  end

  @impl true
  def handle_info({port, {:data, {:eol, bytes}}}, %{port: port} = state) do
    feed_stdin(state, IO.iodata_to_binary([bytes, "\n"]))
  end

  def handle_info({port, {:data, {:noeol, bytes}}}, %{port: port} = state) do
    feed_stdin(state, bytes)
  end

  def handle_info({port, :eof}, %{port: port} = state) do
    emit_frame_events(self(), elem(Stdio.finish(state.framer), 2))
    {:noreply, %{state | stdin_eof: true}}
  end

  def handle_info({:EXIT, port, :normal}, %{port: port, stdin_eof: true} = state) do
    {:noreply, state}
  end

  def handle_info({:EXIT, port, reason}, %{port: port} = state) do
    {:stop, {:stdin_port, reason}, state}
  end

  # ---------------------------------------------------------------------------
  # MCP method handlers
  # ---------------------------------------------------------------------------

  defp validated_request(request) do
    case JsonRpc.classify(request) do
      {:notification, _method, _params} ->
        nil

      {:error, response} ->
        response

      {:request, _method, _id, _params} ->
        case JsonRpc.validate_method(request) do
          {:ok, _method, _id, _params} -> {:request, request}
          {:error, response} -> response
        end
    end
  end

  defp handle_request(%{"method" => "initialize", "id" => id}, _state) do
    %{
      "jsonrpc" => "2.0",
      "id" => id,
      "result" => %{
        "protocolVersion" => @protocol_version,
        "serverInfo" => %{"name" => @server_name, "version" => server_version()},
        "capabilities" => %{"tools" => %{}}
      }
    }
  end

  defp handle_request(%{"method" => "tools/list", "id" => id}, _state) do
    %{"jsonrpc" => "2.0", "id" => id, "result" => %{"tools" => tools()}}
  end

  defp handle_request(
         %{"method" => "tools/call", "id" => id, "params" => %{"name" => tool} = params},
         state
       ) do
    # B1 / ADR-4: normalise, then validate, before dispatch_tool/3 builds the
    # port request — so neither a missing `arguments` nor an undeclared
    # argument reaches the Rust boundary.
    #
    # With no database there is nothing to call, and "run kb init" is the only
    # actionable answer — so that hint wins over an argument complaint the
    # caller cannot act on yet.
    result =
      with false <- match?(%{db_path: nil}, state),
           {:ok, args} <- tool_args(params),
           :ok <- validate_tool_args(tool, args) do
        dispatch_tool(tool, args, state)
      else
        true -> dispatch_tool(tool, %{}, state)
        {:error, message} -> text_error(message)
      end

    %{"jsonrpc" => "2.0", "id" => id, "result" => result}
  end

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

  defp dispatch_tool(tool, args, _state) do
    case AgenticKbMcp.PortRequest.build(tool, args) do
      {:port, request} ->
        request
        |> Map.put("id", gen_id())
        |> port_call_to_content()

      :rebuild ->
        AgenticKbMcp.PortManager.rebuild_async()

        %{
          "content" => [
            %{
              "type" => "text",
              "text" =>
                "Rebuild started in background. Reads continue normally; writes queue until complete."
            }
          ]
        }

      {:error, :unknown_tool} ->
        text_error("Unknown tool: #{tool}")
    end
  end

  # ---------------------------------------------------------------------------
  # Port call helpers
  # ---------------------------------------------------------------------------

  defp port_call_to_content(req) do
    AgenticKbMcp.PortManager.call_port(req) |> render_result()
  end

  @doc """
  Renders a decoded port response envelope into MCP tool-call content.
  Public so tests can exercise the search/`_meta`/`kb_get` rendering without
  spinning up a live port (N2 discharge: whatever this captures, it renders —
  no dead result keys).
  """
  def render_result(resp) do
    case resp do
      %{"type" => "result", "entries" => entries} ->
        meta = Map.get(resp, "_meta")
        %{"content" => [%{"type" => "text", "text" => format_entries(entries, meta)}]}

      %{"type" => "result", "entry" => entry} ->
        %{"content" => [%{"type" => "text", "text" => format_full_entry(entry)}]}

      %{
        "type" => "result",
        "citation_path" => path,
        "citation_sha" => sha,
        "citation_hash" => hash,
        "file_size" => size
      } ->
        text =
          "citation_path=#{path}\ncitation_sha=#{render_scalar(sha)}\n" <>
            "citation_hash=#{hash}\nfile_size=#{size}"

        %{"content" => [%{"type" => "text", "text" => text}]}

      %{"type" => "ok", "imported" => imported, "skipped" => skipped} ->
        %{
          "content" => [
            %{"type" => "text", "text" => "Imported #{imported} entries (#{skipped} skipped)."}
          ]
        }

      %{"type" => "ok", "embedded" => embedded} = resp ->
        parts = ["Re-embedded #{embedded} entries."]
        parts = if resp["failed"], do: parts ++ ["#{resp["failed"]} failed."], else: parts

        parts =
          if resp["skipped"],
            do: parts ++ ["#{resp["skipped"]} skipped (too large)."],
            else: parts

        parts =
          if resp["missing"], do: parts ++ ["#{resp["missing"]} missing embeddings."], else: parts

        parts = if resp["raced"], do: parts ++ ["#{resp["raced"]} raced."], else: parts

        parts =
          case resp["failures"] do
            [_ | _] = failures ->
              causes =
                Enum.map_join(failures, ", ", fn f -> "#{f["id"]}: #{f["cause"]}" end)

              parts ++ ["Causes: #{causes}."]

            _ ->
              parts
          end

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

      %{"type" => "ok", "entry_id" => entry_id} = resp ->
        similar =
          (resp["similar_existing"] || [])
          |> Enum.map_join("\n", fn entry ->
            "- id=#{entry["id"]} path=#{entry["path"]} summary=#{entry["summary"]} score=#{entry["score"]}"
          end)

        text =
          if similar == "",
            do: "Added entry #{entry_id}.",
            else: "Added entry #{entry_id}.\n\nSimilar existing entries:\n#{similar}"

        %{"content" => [%{"type" => "text", "text" => text}]}

      %{"type" => "ok", "run_id" => run_id, "samples" => samples} ->
        text =
          "Audit run #{run_id}: #{length(samples)} sample(s).\n" <>
            Enum.map_join(samples, "\n", fn sample ->
              "- id=#{sample["id"]} path=#{sample["path"]} summary=#{sample["summary"]} kind=#{sample["kind"]} evidence_status=#{sample["evidence_status"]} arm=#{sample["arm"]} evidence=#{json_encode!(sample["evidence"])}"
            end)

        %{"content" => [%{"type" => "text", "text" => String.trim_trailing(text)}]}

      %{"type" => "ok", "recorded" => recorded, "expired" => expired} ->
        %{
          "content" => [
            %{
              "type" => "text",
              "text" => "Recorded #{recorded} audit verdict(s); expired #{expired} entry/entries."
            }
          ]
        }

      %{"type" => "result", "per_kind_session_precision" => rows, "total_runs" => total} = resp ->
        text =
          "Audit report: #{total} recorded verdict(s); last_run_at=#{render_scalar(resp["last_run_at"])}\n" <>
            "per_kind_session_precision=#{json_encode!(rows)}\n" <>
            "per_arm_precision=#{json_encode!(resp["per_arm_precision"])}" <>
            if(Map.has_key?(resp, "injection_telemetry"),
              do: "\ninjection_telemetry=#{json_encode!(resp["injection_telemetry"])}",
              else: ""
            )

        %{"content" => [%{"type" => "text", "text" => text}]}

      %{"type" => "result", "roots" => roots, "graph" => graph, "truncated" => truncated} ->
        text =
          "Provenance roots: #{Enum.join(roots, ", ")}\n" <>
            "truncated=#{truncated}\n" <>
            Enum.map_join(graph, "\n", fn edge -> "#{edge["from"]} -> #{edge["to"]}" end)

        %{"content" => [%{"type" => "text", "text" => String.trim_trailing(text)}]}

      %{"type" => "ok", "expired" => expired_id} ->
        %{"content" => [%{"type" => "text", "text" => "Expired entry #{expired_id}."}]}

      %{"type" => "ok", "run_id" => run_id, "test_id" => test_id, "result" => result} ->
        %{
          "content" => [
            %{"type" => "text", "text" => "Recorded run #{run_id}: #{test_id} -> #{result}."}
          ]
        }

      %{"type" => "ok", "test_id" => test_id} when not is_nil(test_id) ->
        %{"content" => [%{"type" => "text", "text" => "Added test case #{test_id}."}]}

      %{"type" => "result", "test_cases" => cases, "count" => count} ->
        text =
          if cases == [] do
            "(no test cases)"
          else
            header = "#{count} test case(s):\n\n"

            details =
              Enum.map_join(cases, "\n", fn tc ->
                "#{tc["app"]}/#{tc["name"]}  [#{tc["protocol"]}]  id=#{tc["id"]}"
              end)

            header <> details
          end

        %{"content" => [%{"type" => "text", "text" => text}]}

      %{"type" => "result", "stale" => stale, "checked" => checked} = resp ->
        review = Map.get(resp, "review", [])
        # T5 (br-yyb.6): the rust side now distinguishes "ref unreachable from
        # HEAD" from "no commits since recording" and reports the former in a
        # third bucket. Older rust binaries omit the key — default to [] so
        # the formatter stays backward-compatible during a rolling upgrade.
        unreachable = Map.get(resp, "unreachable", [])

        text =
          cond do
            stale == [] and review == [] and unreachable == [] ->
              "Checked #{checked} file(s): all KB entries are up to date."

            true ->
              parts =
                if stale != [] do
                  [
                    "Found #{length(stale)} stale entry/entries (#{checked} file(s) checked):\n\n" <>
                      Enum.map_join(stale, "\n", fn e ->
                        "STALE [#{e["path"]}] #{e["summary"]}  id=#{e["id"]}  recorded-at=#{e["version_ref"]}  (#{e["commits_behind"]} commit(s) ago)"
                      end)
                  ]
                else
                  []
                end

              parts =
                if review != [] do
                  parts ++
                    [
                      "Found #{length(review)} entry/entries for review (matched blame/commits):\n\n" <>
                        Enum.map_join(review, "\n", fn e ->
                          "REVIEW [#{e["path"]}] #{e["summary"]}  id=#{e["id"]}  recorded-at=#{e["version_ref"]}"
                        end)
                    ]
                else
                  parts
                end

              parts =
                if unreachable != [] do
                  parts ++
                    [
                      "Found #{length(unreachable)} entry/entries with unreachable version_ref (recorded at a commit not reachable from current HEAD — deleted branch, GC, or orphan-branch KB):\n\n" <>
                        Enum.map_join(unreachable, "\n", fn e ->
                          "UNKNOWN [#{e["path"]}] #{e["summary"]}  id=#{e["id"]}  recorded-at=#{e["version_ref"]}"
                        end)
                    ]
                else
                  parts
                end

              Enum.join(parts, "\n\n")
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

  def format_entries(entries, meta \\ nil)

  def format_entries([], meta), do: format_meta_header(meta) <> "(no results)"

  def format_entries(entries, meta) do
    {rendered_entries, _bytes_used} =
      Enum.reduce(entries, {[], 0}, fn entry, {acc, bytes_used} ->
        rendered_entry = format_entry(entry)
        separator = if acc == [], do: "", else: "\n\n---\n\n"
        candidate = separator <> rendered_entry
        candidate_bytes = byte_size(candidate)

        if bytes_used + candidate_bytes <= @format_entries_max_bytes do
          {[candidate | acc], bytes_used + candidate_bytes}
        else
          {acc, bytes_used}
        end
      end)
      |> then(fn {acc, bytes_used} -> {Enum.reverse(acc), bytes_used} end)

    format_meta_header(meta) <> finalize_rendered_entries(rendered_entries, length(entries))
  end

  # _meta is a sibling of "entries" on the result envelope (N2): index age
  # plus a STALE WARNING line, shown only when stale_warning is true. Absent
  # or non-map meta (older Rust binary, expand_ids mode) renders nothing.
  defp format_meta_header(meta) when is_map(meta) do
    age_line = "index age: #{render_index_age(meta["index_age"])}"

    lines =
      if meta["stale_warning"] == true do
        [age_line, "STALE WARNING: one or more cited files changed after this entry was indexed"]
      else
        [age_line]
      end

    Enum.join(lines, "\n") <> "\n\n"
  end

  defp format_meta_header(_meta), do: ""

  defp render_index_age(nil), do: "unknown"
  defp render_index_age(seconds) when is_integer(seconds), do: "#{seconds}s"
  defp render_index_age(seconds), do: to_string(seconds)

  defp finalize_rendered_entries(rendered_entries, total_entries) do
    omitted_count = max(total_entries - length(rendered_entries), 0)
    text = Enum.join(rendered_entries, "")

    cond do
      omitted_count == 0 ->
        text

      text == "" ->
        "…(#{omitted_count} more entries omitted)"

      byte_size(text <> omission_suffix(omitted_count)) <= @format_entries_max_bytes ->
        text <> omission_suffix(omitted_count)

      true ->
        rendered_entries
        |> Enum.drop(-1)
        |> finalize_rendered_entries(total_entries)
    end
  end

  defp omission_suffix(omitted_count), do: "\n\n…(#{omitted_count} more entries omitted)"

  # Search results are truncated: summary + first paragraph of content only.
  # Full content lives behind kb_get, keyed by the [kb#<id>] marker below.
  defp format_entry(entry) do
    path = entry["path"] || ""
    summary = entry["summary"] || ""
    content = entry["content"] || ""
    first_para = first_paragraph(content)
    score_str = format_score(entry["score"])
    id = entry["id"] || ""
    confidence = render_scalar(entry["confidence"])
    audit_n = render_scalar(entry["audit_n"])

    sections = [
      "## #{path}#{score_str}",
      "[kb##{id}]",
      "confidence: #{confidence}  audit_n: #{audit_n}",
      summary,
      first_para,
      "full entry: kb_get"
    ]

    case format_evidence(entry["evidence"]) do
      nil -> Enum.join(sections, "\n\n")
      evidence -> Enum.join(sections ++ [evidence], "\n\n")
    end
  end

  # A paragraph is content up to the first blank line; the rest is withheld
  # until kb_get.
  defp first_paragraph(content) do
    content
    |> String.split(~r/\r?\n[ \t]*\r?\n/, parts: 2)
    |> List.first()
  end

  defp format_score(score) when is_number(score), do: " (score: #{Float.round(score * 1.0, 3)})"
  defp format_score(_score), do: ""

  defp format_evidence(evidence) when evidence in [nil, []], do: nil

  defp format_evidence(evidence) when is_list(evidence) do
    evidence_lines =
      evidence
      |> Enum.with_index()
      |> Enum.filter(fn {row, index} ->
        raw_status(row) != "deferred" or index < @evidence_preview_limit
      end)
      |> Enum.map(fn {row, _index} ->
        kind = row["kind"] || ""
        citation_path = row["citation_path"] || ""
        "- kind=#{kind}  citation_path=#{citation_path}  status=#{render_status(row)}"
      end)

    if evidence_lines == [] do
      nil
    else
      Enum.join(["evidence:" | evidence_lines], "\n")
    end
  end

  defp format_evidence(_evidence), do: nil

  # Canonical status string. Prefers the wire's "status" field
  # (verified/relocated/unverified/deferred); falls back to the legacy
  # "verified" tri-state for an older Rust binary that hasn't shipped it yet.
  defp raw_status(%{"status" => status}) when is_binary(status), do: status
  defp raw_status(%{"verified" => true}), do: "verified"
  defp raw_status(%{"verified" => false}), do: "unverified"
  defp raw_status(_row), do: "deferred"

  # BROKEN is shown only when the row is distinguishably a hash mismatch
  # (status=unverified AND verified=false). An ambiguous unverified row
  # (e.g. non-unique relocation match, verified=nil) renders "unverified"
  # verbatim rather than implying a confirmed break. "deferred" is not a
  # failure — it renders as-is.
  defp render_status(row) do
    case {raw_status(row), row["verified"]} do
      {"unverified", false} -> "BROKEN"
      {status, _verified} -> status
    end
  end

  defp render_scalar(nil), do: ""
  defp render_scalar(value), do: to_string(value)

  # ---------------------------------------------------------------------------
  # kb_get: full entry rendering (no truncation, full evidence incl. excerpts)
  # ---------------------------------------------------------------------------

  defp format_full_entry(entry) do
    tags = (entry["tags"] || []) |> Enum.join(", ")

    fields = [
      "[kb##{entry["id"]}]",
      "path: #{entry["path"]}",
      "kind: #{entry["kind"]}  evidence_status: #{entry["evidence_status"]}",
      "version_ref: #{entry["version_ref"]}  is_stale: #{render_scalar(entry["is_stale"])}  permanent: #{render_scalar(entry["permanent"])}",
      "created_at: #{entry["created_at"]}  updated_at: #{entry["updated_at"]}",
      "tags: #{tags}",
      entry["summary"],
      entry["content"]
    ]

    case format_full_evidence(entry["evidence"]) do
      nil -> Enum.join(fields, "\n\n")
      evidence -> Enum.join(fields ++ [evidence], "\n\n")
    end
  end

  defp format_full_evidence(evidence) when evidence in [nil, []], do: nil

  defp format_full_evidence(evidence) when is_list(evidence) do
    evidence_lines =
      Enum.map(evidence, fn row ->
        "- id=#{row["id"]}  kind=#{row["kind"]}  citation_path=#{row["citation_path"]}\n" <>
          "  citation_sha=#{row["citation_sha"]}  citation_hash=#{row["citation_hash"]}  derived_from=#{render_scalar(row["derived_from"])}  recorded_at=#{row["recorded_at"]}\n" <>
          "  excerpt: #{neutralize_excerpt(row["citation_excerpt"])}"
      end)

    Enum.join(["evidence:" | evidence_lines], "\n")
  end

  defp format_full_evidence(_evidence), do: nil

  @excerpt_open "<<UNTRUSTED_EXCERPT>>"
  @excerpt_close "<<END>>"

  # Match the Rust wire boundary without changing its legitimate outer markers:
  # U+200B breaks embedded delimiters, while removal restores the source text.
  defp neutralize_excerpt(nil), do: ""

  defp neutralize_excerpt(excerpt) do
    text = to_string(excerpt)

    if String.starts_with?(text, @excerpt_open) and String.ends_with?(text, @excerpt_close) do
      body_bytes = byte_size(text) - byte_size(@excerpt_open) - byte_size(@excerpt_close)
      body = binary_part(text, byte_size(@excerpt_open), body_bytes)
      @excerpt_open <> String.replace(body, "<<", "<\u200B<") <> @excerpt_close
    else
      String.replace(text, "<<", "<\u200B<")
    end
  end

  defp text_error(msg) do
    %{"content" => [%{"type" => "text", "text" => msg}], "isError" => true}
  end

  # ---------------------------------------------------------------------------
  # The escript starts the VM with `-noinput`, leaving this GenServer as the
  # sole owner of fd 0. The native line port supplies bounded physical chunks
  # while Stdio retains the protocol framing contract.
  # ---------------------------------------------------------------------------

  defp open_stdin_port do
    Port.open({:fd, 0, 0}, [:binary, :eof, :in, {:line, Transport.max_frame_bytes()}])
  end

  defp feed_stdin(state, bytes) do
    {:ok, framer, events} = Stdio.feed(state.framer, bytes)
    emit_frame_events(self(), events)
    {:noreply, %{state | framer: framer}}
  end

  defp emit_frame_events(server, events) do
    Enum.each(events, fn
      {:line, line} -> GenServer.cast(server, {:line, line})
      :frame_too_large -> GenServer.cast(server, :frame_too_large)
      :eof -> GenServer.cast(server, :eof)
    end)
  end

  # ---------------------------------------------------------------------------
  # Utility
  # ---------------------------------------------------------------------------

  defp write_response(response) do
    IO.binwrite(:stdio, [json_encode!(response), "\n"])
  end

  # A missing `arguments` key decodes to nil; an explicit JSON `null` decodes
  # to the atom `:null`, because that is what OTP's `:json` yields — NOT nil,
  # so `Map.get(params, "arguments") || %{}` would not catch it. Both mean "no
  # arguments". Anything else that is not an object is a client error rather
  # than something to coerce (ADR-4).
  defp tool_args(params) do
    case Map.get(params, "arguments") do
      args when args in [nil, :null] -> {:ok, %{}}
      args when is_map(args) -> {:ok, args}
      other -> {:error, "arguments must be a JSON object (got #{inspect(other)})"}
    end
  end

  # Public (not just `defp`) so PortManager's correlation tests can assert
  # uniqueness directly (bd-21ef.2.8, ADR-3 rule 2).
  @doc false
  def gen_id do
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

  defp server_version do
    :agentic_kb_mcp
    |> Application.spec(:vsn)
    |> to_string()
  end
end
