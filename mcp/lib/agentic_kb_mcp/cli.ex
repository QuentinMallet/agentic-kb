defmodule AgenticKbMcp.CLI do
  @moduledoc """
  Escript entry point. Starts the OTP supervision tree then blocks until EOF.
  """

  def main(_args) do
    Application.ensure_all_started(:agentic_kb_mcp)
    # Block forever; McpServer calls System.halt(0) on stdin EOF.
    receive do
    end
  end
end
