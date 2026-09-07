defmodule AgenticKbMcp.Application do
  @moduledoc false

  use Application

  # Evaluated at compile time (baked into the .beam) — no runtime Mix
  # dependency, so this stays safe inside the escript release.
  @start_stdio_children Mix.env() != :test

  @impl true
  def start(_type, _args) do
    if @start_stdio_children do
      start_stdio_children()
    else
      Supervisor.start_link([], strategy: :one_for_one, name: AgenticKbMcp.Supervisor)
    end
  end

  # `AgenticKbMcp.McpServer` spawns a Task that reads stdin line-by-line and
  # calls `System.halt/1` on EOF or error (see `read_stdin/1`). Under `mix
  # test`, Mix starts this OTP application automatically (via `mod:` in
  # mix.exs) with stdin typically closed/non-interactive, which halts the
  # whole BEAM before ExUnit can run or report anything — a silent, falsely
  # "green" 0-test run. The test suite only exercises this module's pure
  # rendering functions and never depends on the supervised process tree, so
  # skipping it in :test is behavior-preserving for the real CLI entry point
  # (`AgenticKbMcp.CLI.main/1`), which always runs with `Mix.env() == :prod`.
  defp start_stdio_children do
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

    caller_id = Application.get_env(:agentic_kb_mcp, :launch_caller_id)
    policy_dir = Application.app_dir(:agentic_kb_mcp, "priv/policies")

    children =
      if db_path do
        [
          {AgenticKbMcp.PortManager, db_path: db_path, kb_bin: kb_bin},
          {AgenticKbMcp.Authorization,
           name: AgenticKbMcp.Authorization,
           caller_id: caller_id,
           opa_opts: [policy_dir: policy_dir]},
          {AgenticKbMcp.McpServer, db_path: db_path, authorization: AgenticKbMcp.Authorization}
        ]
      else
        [
          {AgenticKbMcp.Authorization,
           name: AgenticKbMcp.Authorization,
           caller_id: caller_id,
           opa_opts: [policy_dir: policy_dir]},
          {AgenticKbMcp.McpServer, db_path: nil, authorization: AgenticKbMcp.Authorization}
        ]
      end

    Supervisor.start_link(children, strategy: :one_for_one, name: AgenticKbMcp.Supervisor)
  end
end
