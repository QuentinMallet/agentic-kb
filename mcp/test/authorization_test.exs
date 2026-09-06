defmodule AgenticKbMcp.AuthorizationTest do
  @moduledoc """
  Boundary tests for bd-1orr's launch-bound authorization layer.

  These tests deliberately exercise the authorization service without an MCP
  client: MCP request data is attacker controlled, whereas `caller_id` is
  supplied once by the host when the BEAM is launched.  The service under test
  must therefore never derive its principal from a tool argument or an
  `initialize.clientInfo` value.
  """

  use ExUnit.Case, async: false

  alias AgenticKbMcp.Authorization
  alias AgenticKbMcp.RateLimiter

  defp unique_name(tag), do: :"authz_#{tag}_#{System.unique_integer([:positive, :monotonic])}"

  defp clock_agent(initial_ms \\ 0) do
    {:ok, clock} = Agent.start_link(fn -> initial_ms end)
    {clock, fn -> Agent.get(clock, & &1) end}
  end

  defp start_limiter(clock, limits) do
    name = unique_name(:limiter)

    start_supervised!(
      {RateLimiter, name: name, clock: clock, limits: limits},
      id: name
    )

    name
  end

  defp start_authorizer(opts) do
    name = unique_name(:authorizer)
    start_supervised!({Authorization, Keyword.put(opts, :name, name)}, id: name)
    name
  end

  test "launch principal is immutable and client-supplied identity never reaches policy input" do
    test_pid = self()
    {_clock, monotonic_ms} = clock_agent()

    authorizer =
      start_authorizer(
        caller_id: "host-agent-a",
        clock: monotonic_ms,
        opa: fn input, _deadline_ms ->
          send(test_pid, {:policy_input, input})
          {:ok, true}
        end
      )

    assert :ok =
             Authorization.authorize(authorizer, "kb.audit.run", %{
               "caller_id" => "attacker-b",
               "clientInfo" => %{"name" => "attacker-b"},
               "mode" => "uniform"
             })

    assert_receive {:policy_input, input}
    assert input["caller"] == "host-agent-a"
    refute Map.has_key?(input, "caller_id")
    refute Map.has_key?(input, "clientInfo")
  end

  test "a missing launch principal fails closed before policy evaluation" do
    test_pid = self()
    {_clock, monotonic_ms} = clock_agent()

    authorizer =
      start_authorizer(
        caller_id: nil,
        clock: monotonic_ms,
        opa: fn _input, _deadline_ms ->
          send(test_pid, :policy_was_called)
          {:ok, true}
        end
      )

    assert {:error, :unknown_caller} = Authorization.authorize(authorizer, "kb.audit.run", %{})
    refute_receive :policy_was_called
  end

  for {name, response} <- [
        {:missing, {:error, :missing}},
        {:runtime_error, {:error, :runtime}},
        {:undefined, {:ok, nil}}
      ] do
    test "OPA #{name} fails closed" do
      {_clock, monotonic_ms} = clock_agent()

      authorizer =
        start_authorizer(caller_id: "host-agent-a", clock: monotonic_ms, opa: fn _, _ -> unquote(Macro.escape(response)) end)

      assert {:error, :policy_unavailable} =
               Authorization.authorize(authorizer, "kb.audit.record", %{})
    end
  end

  test "OPA absolute deadline fails closed without waiting for a late answer" do
    {_clock, monotonic_ms} = clock_agent()

    authorizer =
      start_authorizer(
        caller_id: "host-agent-a",
        clock: monotonic_ms,
        opa_timeout_ms: 5,
        opa: fn _input, _deadline_ms ->
          Process.sleep(100)
          {:ok, true}
        end
      )

    started = System.monotonic_time(:millisecond)
    assert {:error, :policy_unavailable} = Authorization.authorize(authorizer, "kb.audit.record", %{})
    assert System.monotonic_time(:millisecond) - started < 75
  end

  test "policy scopes remain separate for uniform audit, traffic audit, record, and expires" do
    {_clock, monotonic_ms} = clock_agent()

    authorizer =
      start_authorizer(
        caller_id: "host-agent-a",
        clock: monotonic_ms,
        opa: fn %{"action" => action}, _ -> {:ok, action in ["kb.audit.run", "kb.audit.record"]} end
      )

    assert :ok = Authorization.authorize(authorizer, "kb.audit.run", %{"mode" => "uniform"})
    assert :ok = Authorization.authorize(authorizer, "kb.audit.record", %{})
    assert {:error, :policy_denied} =
             Authorization.authorize(authorizer, "kb.audit.traffic", %{"mode" => "traffic"})

    assert {:error, :policy_denied} = Authorization.authorize(authorizer, "kb.audit.expire", %{})
    assert {:error, :policy_denied} = Authorization.authorize(authorizer, "kb.entry.expire", %{})
    assert {:error, :policy_denied} = Authorization.authorize(authorizer, "kb.entry.expire.force", %{})
  end

  test "per-caller action buckets isolate callers and make traffic stricter" do
    {_clock, monotonic_ms} = clock_agent()

    limiter =
      start_limiter(monotonic_ms, %{
        "kb.audit.run" => %{limit: 2, window_ms: 1_000},
        "kb.audit.traffic" => %{limit: 1, window_ms: 1_000}
      })

    allow = fn _, _ -> {:ok, true} end

    caller_a =
      start_authorizer(caller_id: "host-agent-a", clock: monotonic_ms, limiter: limiter, opa: allow)

    caller_b =
      start_authorizer(caller_id: "host-agent-b", clock: monotonic_ms, limiter: limiter, opa: allow)

    assert :ok = Authorization.authorize(caller_a, "kb.audit.run", %{})
    assert :ok = Authorization.authorize(caller_a, "kb.audit.run", %{})
    assert {:error, :rate_limited} = Authorization.authorize(caller_a, "kb.audit.run", %{})

    assert :ok = Authorization.authorize(caller_b, "kb.audit.run", %{})

    assert :ok = Authorization.authorize(caller_a, "kb.audit.traffic", %{"mode" => "traffic"})
    assert {:error, :rate_limited} =
             Authorization.authorize(caller_a, "kb.audit.traffic", %{"mode" => "traffic"})
  end
end
