defmodule AgenticKbMcp.PortManager do
  @moduledoc """
  GenServer that owns the Erlang port to the Rust `kb mcp --db <path>` process.
  Serialises all requests through a single port (no multiplexing needed — the
  port protocol is strictly request-response per the spec).

  ## Conformance with `PortProtocol.tla`

  Implements ADR-3 in `.state/.omc/plans/c2-exclusion-boundary.md`; see
  `.state/agent-kb/tla/PortProtocol-counterexample.md` for the checked
  counterexamples (CE1-CE3) this design closes. Per spec action:

    * `Send` — `handle_call({:request, ...}, ...)` builds the line and calls
      `Port.command/2`; `id` becomes the value `collect_response/4` compares
      every reply against (rule 2).
    * `Reply` (enqueue) — no Elixir-side change: the OS pipe is still the
      mailbox the model abstracts.
    * `Reply` (consume, match) — `collect_response/4`'s `%{"id" => ^id}`
      clause.
    * `Reply` (consume, discard) — `collect_response/4`'s `%{"id" => _other}`
      clause: logs at warn with both ids, increments `discarded`, and
      recurses without resetting the absolute deadline (rule 2, rule 3).
    * `Progress` — the `%{"type" => "progress"}` clause recurses against the
      *same* `deadline_ms` computed once at the start of the call, so a tick
      consumes budget instead of resetting it (rule 3).
    * `Timeout` — the `after remaining ->` clause fires once the absolute
      deadline is reached (`remaining` is recomputed from `deadline_ms` on
      every recursion), not "no message for `timeout` ms" (rule 3).
    * `PortCrash` — `init/1` sets `Process.flag(:trap_exit, true)` and
      `Port.open` requests `:exit_status`. `CrashIsPrompt`'s environment
      assumption (crash is observable) maps to the `{^port, {:exit_status,
      _}}` clause in `collect_response/4` (primary mechanism, checked
      mid-call); `handle_info`'s `{port, :closed}` / `{:EXIT, port, reason}`
      clauses are retained unchanged as the `trap_exit` complement, reachable
      whenever the port dies while no call is in flight (rule 1, rule 5).
      Either path returns `port_closed` and `handle_call` replies via
      `{:stop, :port_closed, reply, state}` — a valid return that replies
      before terminating.
    * `Restart` — not implemented here, by design: a restart is the OTP
      supervisor relaunching this crashed GenServer, entirely outside its own
      code (rule 7 governs what may trigger a restart — never a client
      timeout — not whether a supervisor may later restart a port that
      crashed for its own, unrelated reason).

  Rules 4-6 are out of `PortProtocol.tla`'s model scope (single-client, no
  BEAM call/timer semantics, no startup-handshake state) and are covered by
  tests in `test/port_manager_test.exs` instead: `call_port/3` passes
  `:infinity` to `GenServer.call` so the caller's `timeout` is solely the
  inner absolute deadline (rule 4); a caller queued behind a crashing
  request has its resulting `exit` converted to a `port_unavailable`
  envelope by `call_port/3` (rule 5); `await_ready/1` gains matching
  `:exit_status`/`:closed`/`:EXIT` clauses so a startup failure is observed
  immediately rather than via `:handshake_timeout` (rule 6).
  """

  use GenServer
  require Logger

  @handshake_timeout 5_000
  @call_timeout 30_000

  # ---------------------------------------------------------------------------
  # Public API
  # ---------------------------------------------------------------------------

  def start_link(opts) do
    name = Keyword.get(opts, :name, __MODULE__)
    GenServer.start_link(__MODULE__, opts, name: name)
  end

  @doc """
  Send a request map to the Rust port and wait for the response map.

  `server` defaults to the singleton registered name (the real usage in
  production); tests may pass a specific pid/name to talk to an isolated
  instance started with a custom `:name` (see `start_link/1`).
  """
  @spec call_port(map(), timeout(), GenServer.server()) :: map()
  def call_port(request, timeout \\ @call_timeout, server \\ __MODULE__) do
    # ADR-3 rule 4: GenServer.call's own timeout is :infinity — `timeout` is
    # solely the *inner* absolute deadline enforced by collect_response/4.
    # Passing `timeout` to both (as before) makes the two race, and a
    # wedged server outlives the caller's own exit.
    GenServer.call(server, {:request, request, timeout}, :infinity)
  catch
    # ADR-3 rule 5: a caller queued behind a request whose port crashes gets
    # `exit(:noproc)` (or similar) once the manager stops — convert that
    # raised exit into an envelope instead of letting it crash the caller.
    :exit, reason ->
      %{
        "id" => request["id"],
        "type" => "error",
        "code" => "port_unavailable",
        "message" => "port manager unavailable: #{inspect(reason)}"
      }
  end

  @doc """
  Spawn `kb rebuild` as a separate OS process and return immediately.
  The subprocess acquires the file lock, so concurrent writes queue safely.
  Reads through the normal port continue unblocked during the rebuild.
  """
  @spec rebuild_async() :: :ok
  def rebuild_async do
    GenServer.cast(__MODULE__, :rebuild_async)
  end

  # ---------------------------------------------------------------------------
  # GenServer callbacks
  # ---------------------------------------------------------------------------

  @impl true
  def init(opts) do
    db_path = Keyword.fetch!(opts, :db_path)
    kb_bin = Keyword.fetch!(opts, :kb_bin)

    # ADR-3 rule 1: make port death observable. Without both of these, a
    # killed Rust process is invisible until the next Port.command/2 raises
    # ArgumentError — trap_exit covers owner-directed exits (and obliges the
    # terminate/2 below); :exit_status is the primary crash-detection
    # mechanism used mid-call by collect_response/4.
    Process.flag(:trap_exit, true)

    port =
      Port.open({:spawn_executable, kb_bin}, [
        :binary,
        :use_stdio,
        :exit_status,
        {:line, 10_485_760},
        {:args, ["mcp", "--db", db_path]}
      ])

    case await_ready(port) do
      :ok ->
        {:ok, %{port: port, db_path: db_path, kb_bin: kb_bin}}

      {:error, reason} ->
        {:stop, reason}
    end
  end

  @impl true
  def handle_cast(:rebuild_async, %{kb_bin: kb_bin, db_path: db_path} = state) do
    # Derive repo root: <root>/agent-kb/agent-kb.db → <root>
    repo_root = db_path |> Path.dirname() |> Path.dirname()
    log = Path.join(Path.dirname(db_path), "rebuild.log")
    # Shell double-fork: sh (port child) exits immediately after backgrounding
    # kb rebuild. The grandchild is adopted by init and survives BEAM shutdown,
    # so the agent can quit as soon as this cast returns.
    # KB_BIN and LOG are passed via env to avoid shell injection.
    {_output, exit_code} =
      System.cmd("sh", ["-c", ~s("$KB_BIN" rebuild >"$LOG" 2>&1 &)],
        cd: repo_root,
        env: [{"KB_BIN", kb_bin}, {"LOG", log}]
      )

    if exit_code == 0 do
      Logger.info("kb rebuild started in background (log: #{log})")
    else
      Logger.error(
        "kb rebuild: sh exited #{exit_code} — rebuild may not have started (log: #{log})"
      )
    end

    {:noreply, state}
  end

  @impl true
  def handle_call({:request, request, timeout}, _from, %{port: port} = state) do
    line = json_encode!(request) <> "\n"
    Port.command(port, line)
    # ADR-3 rule 3: an absolute monotonic deadline computed once, not a
    # per-receive window — recomputed remaining budget is what makes a
    # progress tick or a discard consume it instead of resetting it.
    deadline_ms = System.monotonic_time(:millisecond) + timeout
    response = collect_response(port, request["id"], deadline_ms)

    case response do
      %{"code" => "port_closed"} ->
        {:stop, :port_closed, response, state}

      _ ->
        {:reply, response, state}
    end
  end

  @impl true
  def handle_info({port, {:data, {:eol, _line}}}, %{port: port} = state) do
    # Unexpected out-of-band data — ignore
    {:noreply, state}
  end

  # New: :exit_status can now arrive here too (idle between calls), not just
  # inside collect_response/4's own receive during an active call.
  def handle_info({port, {:exit_status, status}}, %{port: port} = state) do
    Logger.error("agentic-kb port exited with status #{status}")
    {:stop, :port_closed, state}
  end

  # Retained unchanged (trap_exit complement, ADR-3 rule 1/5) — reachable
  # now that init/1 sets :exit_status/:trap_exit, whereas before neither
  # message ever arrived and these clauses were dead code.
  def handle_info({port, :closed}, %{port: port} = state) do
    Logger.error("agentic-kb port closed unexpectedly")
    {:stop, :port_closed, state}
  end

  def handle_info({:EXIT, port, reason}, %{port: port} = state) do
    Logger.error("agentic-kb port exited: #{inspect(reason)}")
    {:stop, :port_exited, state}
  end

  def handle_info(_msg, state), do: {:noreply, state}

  @impl true
  def terminate(_reason, _state) do
    # ADR-3 consequences: trapping exits obliges stating what happens to the
    # `handle_cast(:rebuild_async, ...)` System.cmd child on shutdown. That
    # child is a double-forked `kb rebuild` background job — System.cmd/3
    # returns as soon as the backgrounding shell itself exits, so by the
    # time rebuild_async/0's cast returns, the actual rebuild process is
    # already adopted by init(1) and detached from this BEAM entirely. It is
    # unaffected by this manager terminating (crash or normal shutdown
    # alike); no cleanup is needed or possible here.
    :ok
  end

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  # ADR-3 rule 6: a startup failure must be observed immediately via
  # :exit_status/:closed/:EXIT, not just via the handshake_timeout fallback —
  # a binary that crashes (or is missing) before printing anything would
  # otherwise report a bare :handshake_timeout instead of the real cause.
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

      {^port, {:exit_status, status}} ->
        {:error, {:startup_exit, status}}

      {^port, :closed} ->
        {:error, :startup_closed}

      {:EXIT, ^port, reason} ->
        {:error, {:startup_exit, reason}}
    after
      @handshake_timeout ->
        {:error, :handshake_timeout}
    end
  end

  # Collect response lines, accumulating progress events until a final,
  # id-matching response (ADR-3 rule 2), bounded by an absolute monotonic
  # deadline that neither a progress tick nor a discarded stale reply resets
  # (ADR-3 rule 3). `discarded` counts non-matching finals so the timeout
  # envelope can report how many were seen.
  defp collect_response(port, id, deadline_ms, discarded \\ 0) do
    remaining = max(deadline_ms - System.monotonic_time(:millisecond), 0)

    receive do
      {^port, {:data, {:eol, line}}} ->
        case json_decode(line) do
          {:ok, %{"type" => "progress"} = prog} ->
            Logger.debug("kb progress: processed=#{prog["processed"]}/#{prog["total"]}")
            collect_response(port, id, deadline_ms, discarded)

          {:ok, %{"id" => ^id} = response} ->
            response

          {:ok, %{"id" => other_id}} ->
            Logger.warning(
              "kb port: discarding stale reply id=#{inspect(other_id)} expected=#{inspect(id)}"
            )

            collect_response(port, id, deadline_ms, discarded + 1)

          {:ok, _response} ->
            %{
              "id" => id,
              "type" => "error",
              "code" => "parse_error",
              "message" => "response missing id: #{line}"
            }

          {:error, _} ->
            %{
              "id" => id,
              "type" => "error",
              "code" => "parse_error",
              "message" => "malformed response: #{line}"
            }
        end

      {^port, {:exit_status, status}} ->
        %{
          "id" => id,
          "type" => "error",
          "code" => "port_closed",
          "message" => "port process exited with status #{status}"
        }

      {^port, :closed} ->
        %{"id" => id, "type" => "error", "code" => "port_closed", "message" => "port closed"}

      {:EXIT, ^port, reason} ->
        %{
          "id" => id,
          "type" => "error",
          "code" => "port_closed",
          "message" => "port exited: #{inspect(reason)}"
        }
    after
      remaining ->
        %{
          "id" => id,
          "type" => "error",
          "code" => "timeout",
          "message" => "port timed out",
          "discarded_responses" => discarded
        }
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
