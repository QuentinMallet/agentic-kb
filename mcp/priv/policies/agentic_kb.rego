package authz

default allow := false

# This bundled policy trusts a single principal out of the box:
# "agentic-kb-host". `default allow := false` makes any other caller
# fail-closed, but operators deploying under a different launch identity
# (a different `--caller-id` value) must replace trusted_callers below with
# their own, or every mutating audit/expiry call will be denied.
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
