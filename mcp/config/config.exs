import Config

# MCP reserves stdout for JSON-RPC frames. Configure OTP's handler before the
# application starts so lifecycle diagnostics cannot corrupt that wire stream.
config :logger, :default_handler, config: %{type: :standard_error}

if config_env() == :test do
  import_config "test.exs"
end
