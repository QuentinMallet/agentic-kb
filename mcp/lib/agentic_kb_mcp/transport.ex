defmodule AgenticKbMcp.Transport do
  @moduledoc false

  @max_frame_bytes 10 * 1024 * 1024

  @spec max_frame_bytes() :: pos_integer()
  def max_frame_bytes, do: @max_frame_bytes
end
