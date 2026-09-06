defmodule AgenticKbMcp.RateLimiter do
  @moduledoc false

  use GenServer

  @default_limits %{
    "kb.audit.run" => %{limit: 20, window_ms: 60_000},
    "kb.audit.traffic" => %{limit: 5, window_ms: 60_000},
    "kb.audit.record" => %{limit: 20, window_ms: 60_000},
    "kb.audit.expire" => %{limit: 10, window_ms: 60_000},
    "kb.entry.expire" => %{limit: 10, window_ms: 60_000},
    "kb.entry.expire.force" => %{limit: 5, window_ms: 60_000}
  }

  def start_link(opts) do
    name = Keyword.get(opts, :name)
    GenServer.start_link(__MODULE__, opts, name: name)
  end

  def check_and_record(server, caller, action) do
    GenServer.call(server, {:check_and_record, caller, action})
  end

  @impl true
  def init(opts) do
    {:ok,
     %{
       clock: Keyword.get(opts, :clock, fn -> System.monotonic_time(:millisecond) end),
       limits: Map.merge(@default_limits, Keyword.get(opts, :limits, %{})),
       buckets: %{}
     }}
  end

  @impl true
  def handle_call({:check_and_record, caller, action}, _from, state) do
    case Map.fetch(state.limits, action) do
      :error ->
        {:reply, {:error, :unknown_action}, state}

      {:ok, %{limit: limit, window_ms: window_ms}} ->
        now = state.clock.()
        key = {caller, action}
        kept = state.buckets |> Map.get(key, []) |> Enum.filter(&(&1 > now - window_ms))

        if length(kept) >= limit do
          {:reply, {:error, :rate_limited}, %{state | buckets: Map.put(state.buckets, key, kept)}}
        else
          {:reply, :ok, %{state | buckets: Map.put(state.buckets, key, [now | kept])}}
        end
    end
  end
end
