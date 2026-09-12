defmodule AgenticKbMcp.CLI do
  @moduledoc """
  Escript entry point. Starts the OTP supervision tree then blocks until EOF.
  """

  def main(args) do
    validate_args!(args)
    case Application.ensure_all_started(:agentic_kb_mcp) do
      {:ok, _} ->
        receive do
        end

      {:error, reason} ->
        IO.puts(:stderr, "agentic-kb-mcp failed to start: #{Exception.message(reason)}")
        System.halt(1)
    end
  end

  defp validate_args!([]), do: :ok

  defp validate_args!(["--caller-id" | _]) do
    raise ArgumentError, "--caller-id is no longer supported"
  end

  defp validate_args!(_), do: raise(ArgumentError, "agentic-kb-mcp does not accept arguments")
end
