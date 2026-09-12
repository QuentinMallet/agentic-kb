defmodule AgenticKbMcp.TransportLimitTest do
  use ExUnit.Case, async: true

  alias AgenticKbMcp.Transport

  test "stdio and Rust port share one 10 MiB line limit" do
    assert Transport.max_frame_bytes() == 10 * 1024 * 1024
    assert AgenticKbMcp.PortManager.line_limit() == Transport.max_frame_bytes()
  end
end
