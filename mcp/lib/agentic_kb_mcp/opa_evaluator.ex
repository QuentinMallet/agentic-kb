defmodule AgenticKbMcp.OpaEvaluator do
  @moduledoc false

  def evaluate(input, opts) do
    opa_bin = Keyword.get(opts, :opa_bin, System.get_env("OPA_BIN") || System.find_executable("opa"))
    policy_dir = Keyword.get(opts, :policy_dir, Path.expand("../priv/policies", __DIR__))
    timeout_ms = Keyword.fetch!(opts, :timeout_ms)

    with true <- is_binary(opa_bin) and File.regular?(opa_bin),
         true <- File.dir?(policy_dir) do
      with {:ok, input_path} <- write_input_file(input) do
        try do
          args = [
            "eval",
            "--strict-builtin-errors",
            "--fail",
            "--format=json",
            "--data",
            policy_dir,
            "--input",
            input_path,
            "--timeout",
            "#{timeout_ms}ms",
            "data.authz.allow"
          ]

          case System.cmd(opa_bin, args, stderr_to_stdout: true) do
            {stdout, 0} -> parse_decision(stdout)
            {_stdout, _status} -> {:error, :runtime}
          end
        after
          File.rm(input_path)
        end
      end
    else
      _ -> {:error, :missing}
    end
  rescue
    _ -> {:error, :runtime}
  end

  def parse_decision(stdout) when is_binary(stdout) do
    case :json.decode(stdout) do
      %{"result" => [%{"expressions" => [%{"value" => true}]}]} -> {:ok, true}
      %{"result" => [%{"expressions" => [%{"value" => false}]}]} -> {:ok, false}
      _ -> {:error, :runtime}
    end
  rescue
    _ -> {:error, :runtime}
  end

  defp write_input_file(input) do
    path = Path.join(System.tmp_dir!(), "agentic-kb-opa-#{System.unique_integer([:positive])}.json")

    case File.write(path, IO.iodata_to_binary(:json.encode(input)) <> "\n") do
      :ok -> {:ok, path}
      {:error, _reason} -> {:error, :runtime}
    end
  end
end
