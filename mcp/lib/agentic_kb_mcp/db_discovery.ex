defmodule AgenticKbMcp.DbDiscovery do
  @moduledoc """
  At each directory, select the canonical .state/agent-kb/agent-kb.db when it
  exists; otherwise select the legacy agent-kb/agent-kb.db, matching Rust
  Paths.discover() candidate order and precedence.
  Roots inside a managed .state git worktree are skipped, mirroring Rust
  Paths.discover().
  """

  @spec discover(String.t()) :: {:ok, String.t()} | {:error, :not_found}
  def discover(start \\ File.cwd!()) do
    do_discover(start)
  end

  defp do_discover(dir) do
    candidates =
      if inside_managed_state_worktree?(dir) do
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

  defp inside_managed_state_worktree?(candidate) do
    candidate
    |> ancestors()
    |> Enum.any?(fn ancestor ->
      state_dir = Path.join(ancestor, ".state")
      gitlink = Path.join([ancestor, ".state", ".git"])

      inside?(candidate, state_dir) and File.regular?(gitlink) and
        case File.read(gitlink) do
          {:ok, "gitdir:" <> _} -> true
          _ -> false
        end
    end)
  end

  defp ancestors(path) do
    case Path.dirname(path) do
      ^path -> [path]
      parent -> [path | ancestors(parent)]
    end
  end

  defp inside?(candidate, directory) do
    candidate == directory or String.starts_with?(candidate, directory <> "/")
  end
end
