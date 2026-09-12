defmodule AgenticKbMcp.Transport.StdioTest do
  use ExUnit.Case, async: true

  alias AgenticKbMcp.Transport.Stdio

  @limit AgenticKbMcp.Transport.max_frame_bytes()

  test "accepts exactly 10 MiB and rejects byte 10 MiB plus one" do
    exact = :binary.copy("x", @limit)
    assert {:ok, _state, [{:line, ^exact}]} = Stdio.feed(Stdio.new(@limit), exact <> "\n")

    assert {:ok, _state, [:frame_too_large]} =
             Stdio.feed(Stdio.new(@limit), exact <> "x\n")
  end

  test "counts UTF-8 and CRLF by bytes and preserves carriage returns" do
    assert {:ok, _state, [{:line, "éé"}]} = Stdio.feed(Stdio.new(4), "éé\n")
    assert {:ok, _state, [:frame_too_large]} = Stdio.feed(Stdio.new(3), "éé\n")
    assert {:ok, _state, [{:line, "ok\r"}]} = Stdio.feed(Stdio.new(3), "ok\r\n")
  end

  test "accepts frames at and below the configured limit" do
    assert {:ok, _state, [{:line, "abc"}]} = Stdio.feed(Stdio.new(3), "abc\n")
    assert {:ok, _state, [{:line, "ab"}]} = Stdio.feed(Stdio.new(3), "ab\n")
  end

  test "rejects one oversized frame then realigns at the next newline" do
    state = Stdio.new(3)
    assert {:ok, state, [:frame_too_large]} = Stdio.feed(state, "abcd")
    assert {:ok, _state, [{:line, "ok"}]} = Stdio.feed(state, " ignored\nok\n")
  end

  test "emits repeated errors then a recovered frame in source order" do
    assert {:ok, state, [:frame_too_large, :frame_too_large, {:line, "ok"}]} =
             Stdio.feed(Stdio.new(3), "abcd\nabcd\nok\n")

    assert {:ok, _state, [:eof]} = Stdio.finish(state)
  end

  test "emits multiple complete frames from one chunk in source order" do
    assert {:ok, _state, [{:line, "one"}, {:line, "two"}, {:line, "three"}]} =
             Stdio.feed(Stdio.new(5), "one\ntwo\nthree\n")
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
