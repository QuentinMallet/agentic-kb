defmodule AgenticKbMcp.OpaEvaluator do
  @moduledoc false

  def evaluate(input, opts) do
    opa_bin =
      Keyword.get(opts, :opa_bin, System.get_env("OPA_BIN") || System.find_executable("opa"))

    policy_dir = Keyword.get(opts, :policy_dir, Path.expand("../priv/policies", __DIR__))
    timeout_ms = Keyword.fetch!(opts, :timeout_ms)

    with true <- is_binary(opa_bin) and File.regular?(opa_bin),
         true <- File.dir?(policy_dir) do
      with {:ok, input_file} <- write_input_file(input, opts) do
        try do
          args = [
            "eval",
            "--strict-builtin-errors",
            "--fail",
            "--format=json",
            "--data",
            policy_dir,
            "--input",
            input_file.path,
            "--timeout",
            "#{timeout_ms}ms",
            "data.authz.allow"
          ]

          case System.cmd(opa_bin, args, stderr_to_stdout: true) do
            {stdout, 0} -> parse_decision(stdout)
            {_stdout, _status} -> {:error, :runtime}
          end
        after
          remove_input_file(input_file)
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

  defp write_input_file(input, opts) do
    parent = Keyword.get(opts, :tmp_dir, System.tmp_dir!())
    forced_suffix = Keyword.get(opts, :tmp_suffix)

    suffixes =
      if forced_suffix do
        [forced_suffix]
      else
        for _ <- 1..8 do
          Base.url_encode64(:crypto.strong_rand_bytes(18), padding: false)
        end
      end

    Enum.reduce_while(suffixes, {:error, :runtime}, fn suffix, _ ->
      dir = Path.join(parent, "agentic-kb-opa-#{suffix}")

      with :ok <- File.mkdir(dir),
           :ok <- File.chmod(dir, 0o700),
           {:ok, path} <- write_exclusive_input(Path.join(dir, "input.json"), input) do
        {:halt, {:ok, %{path: path, dir: dir}}}
      else
        {:error, :eexist} -> {:cont, {:error, :runtime}}
        _ -> {:halt, {:error, :runtime}}
      end
    end)
  end

  defp write_exclusive_input(path, input) do
    bytes = IO.iodata_to_binary(:json.encode(input)) <> "\n"

    case File.open(path, [:write, :exclusive, :binary], fn io ->
           with :ok <- File.chmod(path, 0o600),
                :ok <- IO.binwrite(io, bytes) do
             :ok
           else
             _ -> {:error, :runtime}
           end
         end) do
      {:ok, :ok} -> {:ok, path}
      _ -> {:error, :runtime}
    end
  end

  defp remove_input_file(%{path: path, dir: dir}) do
    File.rm(path)
    File.rmdir(dir)
  end
end
