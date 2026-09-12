defmodule AgenticKbMcp.TestSupport.HeldReader do
  @moduledoc false

  def read(_server) do
    receive do
      :stop -> :ok
    end
  end
end
