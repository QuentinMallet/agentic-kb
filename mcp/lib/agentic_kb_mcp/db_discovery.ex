defmodule AgenticKbMcp.DbDiscovery do
  @moduledoc """
  At each directory, select the canonical .state/agent-kb/agent-kb.db when it
  exists; otherwise a bare `.state/` marker (first run, or a legacy db not yet
  migrated) still selects — and stops at — the canonical path; otherwise
  select the legacy agent-kb/agent-kb.db. This matches Rust Paths.discover()'s
  candidate order and precedence AND its stopping rule: a directory with
  `.state/` but no db file yet must not be walked past in favor of an outer
  ancestor's real database, or the MCP port and the CLI would silently serve
  two different stores for one nested checkout.
  Roots inside a managed .state git worktree are skipped, mirroring Rust
  Paths.discover().
  """

  @spec discover(String.t()) :: {:ok, String.t()} | {:error, :not_found}
  def discover(start \\ File.cwd!()) do
    do_discover(start)
  end

  defp do_discover(dir) do
    if inside_managed_state_worktree?(dir) do
      continue_up(dir)
    else
      canonical = Path.join([dir, ".state", "agent-kb", "agent-kb.db"])
      legacy = Path.join([dir, "agent-kb", "agent-kb.db"])

      selected =
        cond do
          File.exists?(canonical) -> canonical
          File.dir?(Path.join(dir, ".state")) -> canonical
          File.exists?(legacy) -> legacy
          true -> nil
        end

      case selected do
        nil -> continue_up(dir)
        path -> {:ok, path}
      end
    end
  end

  defp continue_up(dir) do
    case Path.dirname(dir) do
      ^dir -> {:error, :not_found}
      parent -> do_discover(parent)
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
