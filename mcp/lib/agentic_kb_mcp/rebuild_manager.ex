defmodule AgenticKbMcp.RebuildManager do
  @moduledoc false

  use GenServer
  require Logger

  @log_limit 65_536

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
       cancel_waiter: nil
     }}
  end

  @impl true
  def handle_call(:request_rebuild, _from, %{phase: phase} = state)
      when phase in [:running, :cancelling, :unknown] do
    {:reply, {:ok, :already_running}, state}
  end

  def handle_call(:request_rebuild, _from, state) do
    case open_rebuild_port(state) do
      {:ok, port, state} ->
        {:reply, {:ok, :started}, %{state | port: port, phase: :running, terminal: nil}}

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
    {:reply, {:ok, terminal}, state}
  end

  def handle_call({:await_terminal, timeout}, from, state) do
    timer = Process.send_after(self(), {:terminal_timeout, from}, timeout)
    {:noreply, %{state | terminal_waiters: [{from, timer} | state.terminal_waiters]}}
  end

  @impl true
  def handle_info({port, {:data, {tag, bytes}}}, %{port: port} = state)
      when tag in [:eol, :noeol] do
    {:noreply, append_log(state, bytes)}
  end

  def handle_info({port, {:exit_status, status}}, %{port: port} = state) do
    terminal = %{exit_status: status, log_tail: state.log_tail}
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
         cancel_waiter: nil
     }}
  end

  def handle_info({:cancel_timeout, from}, %{cancel_waiter: {from, _timer}} = state) do
    GenServer.reply(from, {:error, :timeout})
    {:noreply, %{state | phase: :unknown, cancel_waiter: nil}}
  end

  def handle_info({:cancel_timeout, _from}, state), do: {:noreply, state}

  def handle_info({:terminal_timeout, from}, state) do
    case pop_waiter(state.terminal_waiters, from) do
      {nil, waiters} ->
        {:noreply, %{state | terminal_waiters: waiters}}

      {_waiter, waiters} ->
        GenServer.reply(from, {:error, :timeout})
        {:noreply, %{state | terminal_waiters: waiters}}
    end
  end

  def handle_info({port, :closed}, %{port: port} = state), do: {:noreply, state}
  def handle_info({:EXIT, port, _reason}, %{port: port} = state), do: {:noreply, state}
  def handle_info(_message, state), do: {:noreply, state}

  @impl true
  def terminate(_reason, %{port: nil}), do: :ok

  def terminate(_reason, %{port: port}) do
    _ = safe_command(port, <<3>>)
    Port.close(port)
    :ok
  end

  defp open_rebuild_port(state) do
    try do
      port =
        Port.open({:spawn_executable, state.kb_bin}, [
          :binary,
          :use_stdio,
          :stderr_to_stdout,
          :exit_status,
          {:line, @log_limit},
          {:args, ["rebuild", "--db", state.db_path, "--supervised"]}
        ])

      File.mkdir_p!(Path.dirname(state.log))
      File.write!(state.log, "")
      {:ok, port, %{state | log_tail: ""}}
    rescue
      error in [ArgumentError, ErlangError] -> {:error, Exception.message(error)}
    end
  end

  defp append_log(state, bytes) do
    tail = trim_tail(state.log_tail <> IO.iodata_to_binary(bytes))
    File.write!(state.log, tail)
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

  defp reply_terminal_waiters(waiters, terminal) do
    Enum.each(waiters, fn {from, timer} ->
      Process.cancel_timer(timer)
      GenServer.reply(from, {:ok, terminal})
    end)
  end

  defp reply_cancel_waiter(nil, _status), do: :ok

  defp reply_cancel_waiter({from, timer}, _status) do
    Process.cancel_timer(timer)
    GenServer.reply(from, :ok)
  end

  defp pop_waiter(waiters, from) do
    {found, rest} = Enum.split_with(waiters, fn {candidate, _timer} -> candidate == from end)
    {List.first(found), rest}
  end
end
