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
      port_manager_name = Keyword.get(opts, :port_manager_name, AgenticKbMcp.PortManager)

      [
        {AgenticKbMcp.PortManager, db_path: db_path, kb_bin: kb_bin, name: port_manager_name},
        {AgenticKbMcp.McpServer, mcp_opts}
      ]
    else
      [{AgenticKbMcp.McpServer, mcp_opts}]
    end
  end

  defp startup_opts do
    overrides = Elixir.Application.get_env(:agentic_kb_mcp, :startup_opts, [])
    db_path = Keyword.get_lazy(overrides, :db_path, &discover_db_path/0)

    opts =
      [db_path: db_path]
      |> maybe_put(:reader, Keyword.get(overrides, :reader))
      |> maybe_put(:port_manager_name, Keyword.get(overrides, :port_manager_name))

    if db_path do
      Keyword.put(opts, :kb_bin, Keyword.get_lazy(overrides, :kb_bin, &resolve_kb_bin/0))
    else
      opts
    end
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
