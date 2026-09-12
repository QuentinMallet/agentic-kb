defmodule AgenticKbMcp.Transport.Stdio do
  @moduledoc false

  @enforce_keys [:max_frame]
  defstruct buffer: "", discarding: false, max_frame: nil

  @type event :: {:line, binary()} | :frame_too_large | :eof
  @type t :: %__MODULE__{buffer: binary(), discarding: boolean(), max_frame: pos_integer()}

  @spec new(pos_integer()) :: t()
  def new(max_frame) when is_integer(max_frame) and max_frame > 0 do
    %__MODULE__{max_frame: max_frame}
  end

  @spec feed(t(), binary()) :: {:ok, t(), [event()]}
  def feed(%__MODULE__{} = state, bytes) when is_binary(bytes) do
    consume(state, bytes, [])
  end

  @spec finish(t()) :: {:ok, t(), [event()]}
  def finish(%__MODULE__{discarding: true} = state), do: {:ok, state, [:eof]}
  def finish(%__MODULE__{buffer: ""} = state), do: {:ok, state, [:eof]}

  def finish(%__MODULE__{buffer: buffer} = state) do
    {:ok, %{state | buffer: ""}, [{:line, buffer}, :eof]}
  end

  defp consume(%__MODULE__{discarding: true} = state, bytes, events) do
    case :binary.match(bytes, "\n") do
      :nomatch ->
        {:ok, state, Enum.reverse(events)}

      {newline, 1} ->
        consume(
          %{state | discarding: false},
          binary_part(bytes, newline + 1, byte_size(bytes) - newline - 1),
          events
        )
    end
  end

  defp consume(%__MODULE__{} = state, bytes, events) do
    case :binary.match(bytes, "\n") do
      :nomatch ->
        append_partial(state, bytes, events)

      {newline, 1} ->
        segment = binary_part(bytes, 0, newline)
        rest = binary_part(bytes, newline + 1, byte_size(bytes) - newline - 1)

        if byte_size(state.buffer) + byte_size(segment) <= state.max_frame do
          consume(%{state | buffer: ""}, rest, [{:line, state.buffer <> segment} | events])
        else
          consume(%{state | buffer: ""}, rest, [:frame_too_large | events])
        end
    end
  end

  defp append_partial(state, bytes, events) do
    if byte_size(state.buffer) + byte_size(bytes) <= state.max_frame do
      {:ok, %{state | buffer: state.buffer <> bytes}, Enum.reverse(events)}
    else
      {:ok, %{state | buffer: "", discarding: true}, Enum.reverse([:frame_too_large | events])}
    end
  end
end
