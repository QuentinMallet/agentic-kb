defmodule AgenticKbMcp.OpaPolicyContractTest do
  @moduledoc false

  use ExUnit.Case, async: true

  @policy Path.expand("../priv/policies/agentic_kb.rego", __DIR__)

  test "the shipped Rego policy starts default-deny and names each mutating scope" do
    policy = File.read!(@policy)

    assert policy =~ "default allow := false"

    for scope <- [
          "kb.audit.run",
          "kb.audit.traffic",
          "kb.audit.record",
          "kb.audit.expire",
          "kb.entry.expire",
          "kb.entry.expire.force"
        ] do
      assert policy =~ ~s("#{scope}"), "missing explicit default-deny scope #{scope}"
    end
  end

  test "the shipped policy allows only the trusted launch caller and known scopes" do
    opts = [policy_dir: Path.dirname(@policy), timeout_ms: 1_000]

    assert {:ok, true} =
             AgenticKbMcp.OpaEvaluator.evaluate(
               %{"caller" => "agentic-kb-host", "action" => "kb.audit.record"},
               opts
             )

    assert {:ok, false} =
             AgenticKbMcp.OpaEvaluator.evaluate(
               %{"caller" => "agentic-kb-host", "action" => "kb.unknown"},
               opts
             )

    assert {:ok, false} =
             AgenticKbMcp.OpaEvaluator.evaluate(
               %{"caller" => "attacker", "caller_id" => "agentic-kb-host", "action" => "kb.audit.record"},
               opts
             )
  end
end
