defmodule AgenticKbMcp.Authorization do
  @moduledoc false

  use GenServer

  alias AgenticKbMcp.{OpaEvaluator, RateLimiter}

  def start_link(opts) do
    name = Keyword.get(opts, :name)
    GenServer.start_link(__MODULE__, opts, name: name)
  end

  def authorize(server, action, context) when is_binary(action) and is_map(context) do
    GenServer.call(server, {:authorize, action, context})
  end

  def caller_id(server), do: GenServer.call(server, :caller_id)

  @impl true
  def init(opts) do
    {:ok, task_supervisor} = Task.Supervisor.start_link()
    clock = Keyword.get(opts, :clock, fn -> System.monotonic_time(:millisecond) end)
    caller_id = normalize_caller(Keyword.get(opts, :caller_id))

    limiter =
      Keyword.get_lazy(opts, :limiter, fn ->
        {:ok, pid} = RateLimiter.start_link(clock: clock)
        pid
      end)

    {:ok,
     %{
       caller_id: caller_id,
       clock: clock,
       limiter: limiter,
       task_supervisor: task_supervisor,
       opa_timeout_ms: Keyword.get(opts, :opa_timeout_ms, 1_000),
       opa: Keyword.get(opts, :opa, &OpaEvaluator.evaluate/2),
       opa_opts: Keyword.get(opts, :opa_opts, [])
     }}
  end

  @impl true
  def handle_call({:authorize, _action, _context}, _from, %{caller_id: nil} = state) do
    {:reply, {:error, :unknown_caller}, state}
  end

  def handle_call(:caller_id, _from, state), do: {:reply, state.caller_id, state}

  def handle_call({:authorize, action, context}, _from, state) do
    # Only a process-launch caller can become `input.caller`.  These two keys
    # are protocol/client metadata and must never influence policy or storage.
    input =
      context
      |> Map.drop(["caller_id", "clientInfo"])
      |> Map.put("caller", state.caller_id)
      |> Map.put("action", action)

    case policy_decision(state, input) do
      {:ok, true} ->
        case RateLimiter.check_and_record(state.limiter, state.caller_id, action) do
          :ok -> {:reply, :ok, state}
          {:error, :rate_limited} -> {:reply, {:error, :rate_limited}, state}
          {:error, _} -> {:reply, {:error, :policy_denied}, state}
        end

      {:ok, false} ->
        {:reply, {:error, :policy_denied}, state}

      _ ->
        {:reply, {:error, :policy_unavailable}, state}
    end
  end

  defp policy_decision(state, input) do
    deadline_ms = state.clock.() + state.opa_timeout_ms

    task =
      Task.Supervisor.async_nolink(state.task_supervisor, fn ->
        state.opa.(input, Keyword.merge(state.opa_opts, timeout_ms: state.opa_timeout_ms, deadline_ms: deadline_ms))
      end)

    case Task.yield(task, state.opa_timeout_ms) || Task.shutdown(task, :brutal_kill) do
      {:ok, {:ok, true}} -> {:ok, true}
      {:ok, {:ok, false}} -> {:ok, false}
      _ -> {:error, :unavailable}
    end
  end

  defp normalize_caller(caller) when is_binary(caller) do
    if String.trim(caller) == "", do: nil, else: caller
  end

  defp normalize_caller(_), do: nil
end
