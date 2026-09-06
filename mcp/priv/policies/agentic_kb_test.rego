package authz

test_default_deny if {
  not allow with input as {"caller": "untrusted", "action": "kb.audit.run"}
}

test_trusted_audit_scope if {
  allow with input as {"caller": "agentic-kb-host", "action": "kb.audit.run"}
}

test_unknown_scope_denied if {
  not allow with input as {"caller": "agentic-kb-host", "action": "kb.add"}
}
