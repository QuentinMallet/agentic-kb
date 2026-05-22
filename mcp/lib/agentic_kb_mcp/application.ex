defmodule AgenticKbMcp.Application do
  @moduledoc false

  use Application

  @impl true
  def start(_type, _args) do
    kb_bin =
      System.get_env("KB_BIN") ||
        System.find_executable("kb") ||
        raise "KB_BIN not set and 'kb' not found in PATH"

    db_path =
      case System.get_env("KB_DB_PATH") do
        nil ->
          case AgenticKbMcp.DbDiscovery.discover() do
            {:ok, path} -> path
            {:error, :not_found} -> nil
          end

        path ->
          if File.exists?(path), do: path, else: nil
      end

    children =
      if db_path do
        [
          {AgenticKbMcp.PortManager, db_path: db_path, kb_bin: kb_bin},
          {AgenticKbMcp.McpServer, db_path: db_path}
        ]
      else
        [{AgenticKbMcp.McpServer, db_path: nil}]
      end

    Supervisor.start_link(children, strategy: :one_for_one, name: AgenticKbMcp.Supervisor)
  end
end
