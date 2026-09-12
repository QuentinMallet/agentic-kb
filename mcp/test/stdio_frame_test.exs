defmodule AgenticKbMcp.Transport.StdioTest do
  use ExUnit.Case, async: true

  alias AgenticKbMcp.Transport.Stdio

  test "accepts frames at and below the configured limit" do
    assert {:ok, _state, [{:line, "abc"}]} = Stdio.feed(Stdio.new(3), "abc\n")
    assert {:ok, _state, [{:line, "ab"}]} = Stdio.feed(Stdio.new(3), "ab\n")
  end

  test "rejects one oversized frame then realigns at the next newline" do
    state = Stdio.new(3)
    assert {:ok, state, [:frame_too_large]} = Stdio.feed(state, "abcd")
    assert {:ok, _state, [{:line, "ok"}]} = Stdio.feed(state, " ignored\nok\n")
  end

  test "never dispatches an oversized unterminated frame and emits one error" do
    assert {:ok, state, [:frame_too_large]} = Stdio.feed(Stdio.new(3), "abcd")
    assert {:ok, _state, [:eof]} = Stdio.finish(state)
  end

  test "dispatches one bounded partial frame before EOF" do
    assert {:ok, state, []} = Stdio.feed(Stdio.new(3), "abc")
    assert {:ok, _state, [{:line, "abc"}, :eof]} = Stdio.finish(state)
  end

  test "accepts a short line without waiting for a fixed-size buffer" do
    assert {:ok, _state, [{:line, "short"}]} = Stdio.feed(Stdio.new(10), "short\n")
  end
end
