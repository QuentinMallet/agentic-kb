import Config

fake_port = Path.expand("../test/support/fake_port.sh", __DIR__)
File.chmod!(fake_port, 0o755)

config :agentic_kb_mcp,
  startup_opts: [
    db_path: Path.join(System.tmp_dir!(), "agentic-kb-mcp-default-test.db"),
    kb_bin: fake_port,
    port_manager_name: :agentic_kb_mcp_test_port_manager,
    input_port_factory: &AgenticKbMcp.TestSupport.HeldInput.open/0
  ]
