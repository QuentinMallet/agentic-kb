defmodule AgenticKbMcp.DbDiscovery do
  @moduledoc """
  Walk up from cwd looking for agent-kb/agent-kb.db — mirrors the Rust Paths::discover() logic.
  """

  @spec discover(String.t()) :: {:ok, String.t()} | {:error, :not_found}
  def discover(start \\ File.cwd!()) do
    do_discover(start)
  end

  defp do_discover(dir) do
    candidate = Path.join([dir, "agent-kb", "agent-kb.db"])

    cond do
      File.exists?(candidate) ->
        {:ok, candidate}

      dir == "/" ->
        {:error, :not_found}

      true ->
        do_discover(Path.dirname(dir))
    end
  end
end
