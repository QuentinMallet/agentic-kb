defmodule AgenticKbMcp.LoggerProtocolTest do
  use ExUnit.Case, async: true

  test "the default logger handler writes diagnostics to stderr" do
    assert [config: %{type: :standard_error}] =
             Application.fetch_env!(:logger, :default_handler)
  end
end
