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

  def render_result(resp), do: AgenticKbMcp.Renderer.render_result(resp)

  def format_entries(entries, meta \\ nil),
    do: AgenticKbMcp.Renderer.format_entries(entries, meta)

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
