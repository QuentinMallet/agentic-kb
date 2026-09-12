defmodule AgenticKbMcp.MixProject do
  use Mix.Project

  def project do
    [
      app: :agentic_kb_mcp,
      version: "0.2.0",
      elixir: "~> 1.18",
      start_permanent: Mix.env() == :prod,
      escript: escript(),
      deps: deps()
    ]
  end

  def application do
    [
      extra_applications: [:logger, :crypto],
      mod: {AgenticKbMcp.Application, []}
    ]
  end

  defp escript do
    # CLI argument validation must run before the OTP application starts so a
    # retired launch flag cannot start a server as a side effect.
    [main_module: AgenticKbMcp.CLI, app: nil, emu_args: "-noinput"]
  end

  # Zero external deps — uses OTP 27 :json module
  defp deps, do: []
end
