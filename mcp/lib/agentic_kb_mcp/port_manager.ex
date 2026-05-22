defmodule AgenticKbMcp.PortManager do
  @moduledoc """
  GenServer that owns the Erlang port to the Rust `kb mcp --db <path>` process.
  Serialises all requests through a single port (no multiplexing needed — the
  port protocol is strictly request-response per the spec).
  """

  use GenServer
  require Logger

  @handshake_timeout 5_000
  @call_timeout 30_000

  # ---------------------------------------------------------------------------
  # Public API
  # ---------------------------------------------------------------------------

  def start_link(opts) do
    GenServer.start_link(__MODULE__, opts, name: __MODULE__)
  end

  @doc "Send a request map to the Rust port and wait for the response map."
  @spec call_port(map()) :: map()
  def call_port(request) do
    GenServer.call(__MODULE__, {:request, request}, @call_timeout)
  end

  # ---------------------------------------------------------------------------
  # GenServer callbacks
  # ---------------------------------------------------------------------------

  @impl true
  def init(opts) do
    db_path = Keyword.fetch!(opts, :db_path)
    kb_bin = Keyword.fetch!(opts, :kb_bin)

    port =
      Port.open({:spawn_executable, kb_bin}, [
        :binary,
        :use_stdio,
        {:line, 10_485_760},
        {:args, ["mcp", "--db", db_path]}
      ])

    case await_ready(port) do
      :ok ->
        {:ok, %{port: port, db_path: db_path}}

      {:error, reason} ->
        {:stop, reason}
    end
  end

  @impl true
  def handle_call({:request, request}, _from, %{port: port} = state) do
    line = json_encode!(request) <> "\n"
    Port.command(port, line)
    response = collect_response(port, request["id"])
    {:reply, response, state}
  end

  @impl true
  def handle_info({port, {:data, {:eol, _line}}}, %{port: port} = state) do
    # Unexpected out-of-band data — ignore
    {:noreply, state}
  end

  def handle_info({port, :closed}, %{port: port} = state) do
    Logger.error("agentic-kb port closed unexpectedly")
    {:stop, :port_closed, state}
  end

  def handle_info({:EXIT, port, reason}, %{port: port} = state) do
    Logger.error("agentic-kb port exited: #{inspect(reason)}")
    {:stop, :port_exited, state}
  end

  def handle_info(_msg, state), do: {:noreply, state}

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  defp await_ready(port) do
    receive do
      {^port, {:data, {:eol, line}}} ->
        case json_decode(line) do
          {:ok, %{"type" => "ready"}} ->
            :ok

          {:ok, %{"type" => "error", "message" => msg}} ->
            {:error, {:db_error, msg}}

          _ ->
            {:error, :bad_handshake}
        end
    after
      @handshake_timeout ->
        {:error, :handshake_timeout}
    end
  end

  # Collect response lines, accumulating progress events until a final response.
  defp collect_response(port, id) do
    receive do
      {^port, {:data, {:eol, line}}} ->
        case json_decode(line) do
          {:ok, %{"type" => "progress"} = prog} ->
            Logger.debug("kb progress: processed=#{prog["processed"]}/#{prog["total"]}")
            collect_response(port, id)

          {:ok, response} ->
            response

          {:error, _} ->
            %{
              "id" => id,
              "type" => "error",
              "code" => "parse_error",
              "message" => "malformed response: #{line}"
            }
        end
    after
      @call_timeout ->
        %{"id" => id, "type" => "error", "code" => "timeout", "message" => "port timed out"}
    end
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
