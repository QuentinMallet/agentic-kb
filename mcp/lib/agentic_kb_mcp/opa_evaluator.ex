defmodule AgenticKbMcp.OpaEvaluator do
  @moduledoc false

  # The exit status is the decision authority.  `opa eval --fail` returns 0
  # only for a defined, true boolean result; false, undefined, malformed
  # policy, and strict builtin failures all deny.  Stdout is diagnostics only.
  def evaluate(input, opts) do
    opa_bin = Keyword.get(opts, :opa_bin, System.find_executable("opa"))
    policy_dir = Keyword.get(opts, :policy_dir, Path.expand("../priv/policies", __DIR__))
    timeout_ms = Keyword.fetch!(opts, :timeout_ms)

    with true <- is_binary(opa_bin) and File.regular?(opa_bin),
         true <- File.dir?(policy_dir) do
      args = [
        "eval",
        "--strict-builtin-errors",
        "--fail",
        "--format=json",
        "--data",
        policy_dir,
        "--stdin-input",
        "--timeout",
        "#{timeout_ms}ms",
        "data.authz.allow"
      ]

      port =
        Port.open({:spawn_executable, opa_bin}, [
          :binary,
          :use_stdio,
          :exit_status,
          {:args, args}
        ])

      Port.command(port, :json.encode(input) <> "\n")
      await_exit(port, timeout_ms)
    else
      _ -> {:error, :missing}
    end
  rescue
    _ -> {:error, :runtime}
  end

  defp await_exit(port, timeout_ms) do
    receive do
      {^port, {:data, _}} -> await_exit(port, timeout_ms)
      {^port, {:exit_status, 0}} -> {:ok, true}
      {^port, {:exit_status, _}} -> {:error, :runtime}
      {^port, :closed} -> {:error, :runtime}
    after
      timeout_ms ->
        Port.close(port)
        {:error, :timeout}
    end
  end
end
