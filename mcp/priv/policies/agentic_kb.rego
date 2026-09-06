package authz

default allow := false

# Operators deploy the launch principal in this policy/data boundary.  The
# default policy intentionally authorizes no host, so a missing deployment
# remains fail-closed rather than accidentally granting every local client.
trusted_callers := {"agentic-kb-host"}

mutating_scopes := {
  "kb.audit.run",
  "kb.audit.traffic",
  "kb.audit.record",
  "kb.audit.expire",
  "kb.entry.expire",
  "kb.entry.expire.force",
}

allow if {
  input.caller in trusted_callers
  input.action in mutating_scopes
}
