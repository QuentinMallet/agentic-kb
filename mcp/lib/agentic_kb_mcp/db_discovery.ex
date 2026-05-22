defmodule AgenticKbMcp.DbDiscovery do
  @moduledoc """
  Walk up from cwd looking for agent-kb/agent-kb.db — mirrors the Rust Paths::discover() logic.
  Also checks .state/agent-kb/agent-kb.db at each level (worktree/agentic branch convention).
  """

  @spec discover(String.t()) :: {:ok, String.t()} | {:error, :not_found}
  def discover(start \\ File.cwd!()) do
    do_discover(start)
  end

  defp do_discover(dir) do
    candidates = [
      Path.join([dir, "agent-kb", "agent-kb.db"]),
      Path.join([dir, ".state", "agent-kb", "agent-kb.db"])
    ]

    case Enum.find(candidates, &File.exists?/1) do
      nil when dir == "/" -> {:error, :not_found}
      nil -> do_discover(Path.dirname(dir))
      path -> {:ok, path}
    end
  end
end
