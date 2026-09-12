defmodule AgenticKbMcp.PortRequest do
  @moduledoc false

  def build("kb_search", args),
    do: port("search", args, ~w(query limit mode path_prefix tag inline_verify_k expand_ids))

  def build("kb_add", args),
    do: port("add", args, ~w(path summary content tags permanent replace_path kind evidence cues))

  def build("kb_cite", args), do: port("cite", args, ~w(path start end))
  def build("kb_import", args), do: port("import", args, ~w(path upsert))
  def build("kb_stale_check", args), do: port("stale_check", args, ~w(files commits blame))
  def build("kb_expire", args), do: port("expire", args, ~w(entry_id reason force))
  def build("kb_run", args), do: port("run", args, ~w(test_id result adapter detail))
  def build("kb_test_add", args), do: port("test_add", args, ~w(app name protocol config test_id))
  def build("kb_tests", args), do: port("tests", args, ~w(app))
  def build("kb_reembed", args), do: port("reembed", args, ~w(dry_run max_chars))
  def build("kb_compact", _args), do: {:port, %{"method" => "compact"}}
  def build("kb_rebuild", _args), do: :rebuild
  def build("kb_audit_run", args), do: port("audit_run", args, ~w(sample_size mode))
  def build("kb_audit_record", args), do: port("audit_record", args, ~w(run_id verdicts))
  def build("kb_audit_report", _args), do: {:port, %{"method" => "audit_report"}}
  def build("kb_provenance", args), do: port("provenance", args, ~w(entry_id max_depth))
  def build("kb_get", args), do: port("kb_get", args, ~w(entry_id))
  def build(_tool, _args), do: {:error, :unknown_tool}

  defp port(method, args, fields) do
    request =
      Enum.reduce(fields, %{"method" => method}, fn field, request ->
        case Map.fetch(args, field) do
          {:ok, nil} -> request
          {:ok, value} -> Map.put(request, field, value)
          :error -> request
        end
      end)

    {:port, request}
  end
end
