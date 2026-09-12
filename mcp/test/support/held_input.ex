defmodule AgenticKbMcp.TestSupport.HeldInput do
  @moduledoc false

  def open do
    Port.open({:spawn_executable, System.find_executable("cat")}, [:binary, :use_stdio])
  end
end
