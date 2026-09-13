defmodule AgenticKbMcp.RebuildManager do
  @moduledoc false

  use GenServer
  require Logger

  @log_limit 65_536
  @exit_status_grace 100
  @ready_timeout 500

  def start_link(opts) do
    name = Keyword.get(opts, :name, __MODULE__)
    GenServer.start_link(__MODULE__, opts, name: name)
  end

  @spec request_rebuild(GenServer.server()) ::
          {:ok, :started | :already_running} | {:error, term()}
  def request_rebuild(server \\ __MODULE__), do: GenServer.call(server, :request_rebuild)

  @doc false
  @spec cancel_and_await(GenServer.server(), timeout()) :: :ok | {:error, term()}
  def cancel_and_await(server \\ __MODULE__, timeout \\ 5_000) do
    GenServer.call(server, {:cancel_and_await, timeout}, timeout + 100)
  end

  @doc false
  @spec await_terminal(GenServer.server(), timeout()) :: {:ok, map()} | {:error, :timeout}
  def await_terminal(server \\ __MODULE__, timeout \\ 5_000) do
    GenServer.call(server, {:await_terminal, timeout}, timeout + 100)
  end

  @impl true
  def init(opts) do
    Process.flag(:trap_exit, true)

    db_path = Keyword.fetch!(opts, :db_path)

    {:ok,
     %{
       kb_bin: Keyword.fetch!(opts, :kb_bin),
       db_path: db_path,
       log: Path.join(Path.dirname(db_path), "rebuild.log"),
       port: nil,
       phase: :idle,
       log_tail: "",
       terminal: nil,
       terminal_waiters: [],
       cancel_waiter: nil,
       exit_status_grace_timer: nil,
       launch_waiter: nil,
       ready_timer: nil
     }}
  end

  @impl true
  def handle_call(:request_rebuild, _from, %{phase: :unknown} = state) do
    {:reply, {:error, :rebuild_status_unknown}, state}
  end

  def handle_call(:request_rebuild, _from, %{phase: :starting} = state) do
    {:reply, {:error, :rebuild_starting}, state}
  end

  def handle_call(:request_rebuild, _from, %{phase: phase} = state)
      when phase in [:running, :cancelling] do
    {:reply, {:ok, :already_running}, state}
  end

  def handle_call(:request_rebuild, from, state) do
    case open_rebuild_port(state) do
      {:ok, port, state} ->
        ready_timer = Process.send_after(self(), {:ready_timeout, port}, @ready_timeout)

        {:noreply,
         %{
           state
           | port: port,
             phase: :starting,
             terminal: nil,
             launch_waiter: from,
             ready_timer: ready_timer
         }}

      {:error, reason} ->
        {:reply, {:error, {:launch_failed, reason}}, state}
    end
  end

  def handle_call({:cancel_and_await, _timeout}, _from, %{phase: :idle} = state) do
    {:reply, :ok, state}
  end

  def handle_call({:cancel_and_await, timeout}, from, %{phase: :running, port: port} = state) do
    timer = Process.send_after(self(), {:cancel_timeout, from}, timeout)

    case safe_command(port, <<3>>) do
      :ok -> {:noreply, %{state | phase: :cancelling, cancel_waiter: {from, timer}}}
      {:error, reason} -> {:reply, {:error, reason}, state}
    end
  end

  def handle_call({:cancel_and_await, _timeout}, _from, state) do
    {:reply, {:error, :already_cancelling}, state}
  end

  def handle_call({:await_terminal, _timeout}, _from, %{terminal: terminal} = state)
      when not is_nil(terminal) do
    {:reply, terminal, state}
  end

  def handle_call({:await_terminal, timeout}, from, state) do
    timer = Process.send_after(self(), {:terminal_timeout, from}, timeout)
    {:noreply, %{state | terminal_waiters: [{from, timer} | state.terminal_waiters]}}
  end

  @impl true
  def handle_info({port, {:data, {:eol, bytes}}}, %{port: port, phase: :starting} = state) do
    state = append_log(state, bytes)

    case rebuild_signal(bytes) do
      :ready ->
        reply_launch_waiter(state.launch_waiter, {:ok, :started})
        cancel_timer(state.ready_timer)
        {:noreply, %{state | phase: :running, launch_waiter: nil, ready_timer: nil}}

      {:error, message} ->
        reply_launch_waiter(state.launch_waiter, {:error, {:launch_failed, message}})
        cancel_timer(state.ready_timer)
        {:noreply, %{state | phase: :unknown, launch_waiter: nil, ready_timer: nil}}

      :other ->
        {:noreply, state}
    end
  end

  def handle_info({port, {:data, {tag, bytes}}}, %{port: port} = state)
      when tag in [:eol, :noeol] do
    {:noreply, append_log(state, bytes)}
  end

  def handle_info({port, {:exit_status, status}}, %{port: port} = state) do
    cancel_exit_status_grace(state.exit_status_grace_timer)
    cancel_timer(state.ready_timer)
    reply_launch_waiter(state.launch_waiter, {:error, {:launch_failed, {:exit_status, status}}})
    terminal = {:ok, %{exit_status: status, log_tail: state.log_tail}}
    persist_log_tail(state.log, state.log_tail)
    reply_terminal_waiters(state.terminal_waiters, terminal)
    reply_cancel_waiter(state.cancel_waiter, status)

    level = if status == 0, do: :info, else: :error
    Logger.log(level, "kb rebuild exited with status #{status}")

    {:noreply,
     %{
       state
       | port: nil,
         phase: :idle,
         terminal: terminal,
         terminal_waiters: [],
         cancel_waiter: nil,
         exit_status_grace_timer: nil,
         launch_waiter: nil,
         ready_timer: nil
     }}
  end

  def handle_info({:cancel_timeout, from}, %{cancel_waiter: {from, _timer}} = state) do
    GenServer.reply(from, {:error, :timeout})
    {:noreply, %{state | phase: :unknown, cancel_waiter: nil}}
  end

  def handle_info({:cancel_timeout, _from}, state), do: {:noreply, state}

  def handle_info({:ready_timeout, port}, %{port: port, phase: :starting} = state) do
    reply_launch_waiter(state.launch_waiter, {:error, :ready_timeout})
    cancel_port(port)
    {:noreply, %{state | phase: :unknown, launch_waiter: nil, ready_timer: nil}}
  end

  def handle_info({:ready_timeout, _port}, state), do: {:noreply, state}

  def handle_info({:terminal_timeout, from}, state) do
    case pop_waiter(state.terminal_waiters, from) do
      {nil, waiters} ->
        {:noreply, %{state | terminal_waiters: waiters}}

      {_waiter, waiters} ->
        GenServer.reply(from, {:error, :timeout})
        {:noreply, %{state | terminal_waiters: waiters}}
    end
  end

  def handle_info({port, :closed}, %{port: port} = state), do: await_exit_status(state)
  def handle_info({:EXIT, port, _reason}, %{port: port} = state), do: await_exit_status(state)

  def handle_info(
        {:exit_status_missing, port, token},
        %{port: port, phase: :unknown, exit_status_grace_timer: {_timer, token}} = state
      ) do
    terminal = {:error, :exit_status_missing}
    cancel_timer(state.ready_timer)
    reply_launch_waiter(state.launch_waiter, {:error, {:launch_failed, :exit_status_missing}})
    reply_terminal_waiters(state.terminal_waiters, terminal)
    reply_cancel_waiter(state.cancel_waiter, :exit_status_missing)

    {:noreply,
     %{
       state
       | phase: :unknown,
         terminal: terminal,
         terminal_waiters: [],
         cancel_waiter: nil,
         exit_status_grace_timer: nil,
         launch_waiter: nil,
         ready_timer: nil
     }}
  end

  def handle_info({:exit_status_missing, _port, _token}, state), do: {:noreply, state}

  def handle_info(_message, state), do: {:noreply, state}

  @impl true
  def terminate(_reason, %{port: nil}), do: :ok

  def terminate(_reason, %{port: port}) do
    _ = safe_command(port, <<3>>)
    safe_close(port)
    :ok
  end

  defp open_rebuild_port(state) do
    try do
      port =
        Port.open({:spawn_executable, state.kb_bin}, [
          :binary,
          :use_stdio,
          :exit_status,
          {:line, @log_limit},
          {:args, ["rebuild", "--db", state.db_path, "--supervised"]}
        ])

      {:ok, port, %{state | log_tail: ""}}
    rescue
      error in [ArgumentError, ErlangError] -> {:error, Exception.message(error)}
    end
  end

  defp append_log(state, bytes) do
    tail = trim_tail(state.log_tail <> IO.iodata_to_binary(bytes))
    %{state | log_tail: tail}
  end

  defp trim_tail(bytes) when byte_size(bytes) <= @log_limit, do: bytes
  defp trim_tail(bytes), do: binary_part(bytes, byte_size(bytes) - @log_limit, @log_limit)

  defp safe_command(port, bytes) do
    Port.command(port, bytes)
    :ok
  rescue
    ArgumentError -> {:error, :port_closed}
  end

  defp safe_close(port) do
    Port.close(port)
  rescue
    ArgumentError -> :ok
  end

  defp cancel_port(port) do
    case safe_command(port, <<3>>) do
      :ok -> :ok
      {:error, :port_closed} -> safe_close(port)
    end
  end

  defp reply_terminal_waiters(waiters, terminal) do
    Enum.each(waiters, fn {from, timer} ->
      Process.cancel_timer(timer)
      GenServer.reply(from, terminal)
    end)
  end

  defp reply_cancel_waiter(nil, _status), do: :ok

  defp reply_cancel_waiter({from, timer}, :exit_status_missing) do
    Process.cancel_timer(timer)
    GenServer.reply(from, {:error, :exit_status_missing})
  end

  defp reply_cancel_waiter({from, timer}, _status) do
    Process.cancel_timer(timer)
    GenServer.reply(from, :ok)
  end

  defp await_exit_status(%{phase: :idle} = state), do: {:noreply, state}

  defp await_exit_status(%{exit_status_grace_timer: nil, port: port} = state) do
    token = make_ref()
    timer = Process.send_after(self(), {:exit_status_missing, port, token}, @exit_status_grace)
    {:noreply, %{state | phase: :unknown, exit_status_grace_timer: {timer, token}}}
  end

  defp await_exit_status(state), do: {:noreply, state}

  defp cancel_exit_status_grace(nil), do: :ok
  defp cancel_exit_status_grace({timer, _token}), do: Process.cancel_timer(timer)

  defp cancel_timer(nil), do: :ok
  defp cancel_timer(timer), do: Process.cancel_timer(timer)

  defp reply_launch_waiter(nil, _reply), do: :ok
  defp reply_launch_waiter(from, reply), do: GenServer.reply(from, reply)

  defp persist_log_tail(log, tail) do
    File.mkdir_p!(Path.dirname(log))
    File.write!(log, tail)
  end

  defp rebuild_signal(bytes) do
    case :json.decode(bytes) do
      %{"rebuild" => "ready"} -> :ready
      %{"rebuild" => "error", "message" => message} when is_binary(message) -> {:error, message}
      _ -> :other
    end
  rescue
    _ -> :other
  end

  defp pop_waiter(waiters, from) do
    {found, rest} = Enum.split_with(waiters, fn {candidate, _timer} -> candidate == from end)
    {List.first(found), rest}
  end
end
