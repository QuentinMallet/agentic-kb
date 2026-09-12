defmodule AgenticKbMcp.Application do
  @moduledoc false

  use Application

  @impl true
  def start(_type, _args) do
    Supervisor.start_link(child_specs(startup_opts()),
      strategy: :one_for_one,
      name: AgenticKbMcp.Supervisor
    )
  end

  @doc false
  def child_specs(opts) do
    db_path = Keyword.fetch!(opts, :db_path)
    reader = Keyword.get(opts, :reader)

    mcp_opts =
      [db_path: db_path]
      |> maybe_put(:reader, reader)

    if db_path do
      kb_bin = Keyword.fetch!(opts, :kb_bin)

      [
        {AgenticKbMcp.PortManager, db_path: db_path, kb_bin: kb_bin},
        {AgenticKbMcp.McpServer, mcp_opts}
      ]
    else
      [{AgenticKbMcp.McpServer, mcp_opts}]
    end
  end

  defp startup_opts do
    defaults = [db_path: discover_db_path(), kb_bin: resolve_kb_bin()]
    overrides = Elixir.Application.get_env(:agentic_kb_mcp, :startup_opts, [])
    Keyword.merge(defaults, overrides)
  end

  defp resolve_kb_bin do
    System.get_env("KB_BIN") ||
      System.find_executable("kb") ||
      raise "KB_BIN not set and 'kb' not found in PATH"
  end

  defp discover_db_path do
    case System.get_env("KB_DB_PATH") do
      nil ->
        case AgenticKbMcp.DbDiscovery.discover() do
          {:ok, path} -> path
          {:error, :not_found} -> nil
        end

      path ->
        if File.exists?(path), do: path, else: nil
    end
  end

  defp maybe_put(opts, _key, nil), do: opts
  defp maybe_put(opts, key, value), do: Keyword.put(opts, key, value)
end
