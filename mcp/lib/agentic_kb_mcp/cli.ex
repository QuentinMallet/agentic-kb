defmodule AgenticKbMcp.CLI do
  @moduledoc """
  Escript entry point. Starts the OTP supervision tree then blocks until EOF.
  """

  def main(args) do
    # This is process-launch metadata controlled by the host service/unit.
    # It is read once before the OTP tree starts; MCP JSON-RPC metadata and
    # tool arguments never participate in principal selection.
    Application.put_env(:agentic_kb_mcp, :launch_caller_id, launch_caller(args))
    Application.ensure_all_started(:agentic_kb_mcp)
    # Block forever; McpServer calls System.halt(0) on stdin EOF.
    receive do
    end
  end

  defp launch_caller(["--caller-id", caller]) when is_binary(caller) and byte_size(caller) > 0,
    do: caller

  defp launch_caller(_), do: nil
end
