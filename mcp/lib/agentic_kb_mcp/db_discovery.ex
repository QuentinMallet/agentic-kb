defmodule AgenticKbMcp.DbDiscovery do
  @moduledoc """
  Walk up from cwd looking first for the fleet-ratified canonical
  .state/agent-kb/agent-kb.db, then for the legacy agent-kb/agent-kb.db fallback.
  Roots inside a .state tree are skipped, mirroring Rust Paths.discover().
  """

  @spec discover(String.t()) :: {:ok, String.t()} | {:error, :not_found}
  def discover(start \\ File.cwd!()) do
    do_discover(start)
  end

  defp do_discover(dir) do
    candidates =
      if ".state" in Path.split(dir) do
        []
      else
        [
          Path.join([dir, ".state", "agent-kb", "agent-kb.db"]),
          Path.join([dir, "agent-kb", "agent-kb.db"])
        ]
      end

    case Enum.find(candidates, &File.exists?/1) do
      nil when dir == "/" -> {:error, :not_found}
      nil -> do_discover(Path.dirname(dir))
      path -> {:ok, path}
    end
  end
end
