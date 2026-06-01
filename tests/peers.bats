#!/usr/bin/env bats
# Integration tests for `kb peers` subcommands.
#
# Requires:
#   - bats-core
#   - jq
#   - kb binary at ./result/bin/kb (override with KB env var)
#
# Run after `nix build`:
#   bats tests/peers.bats

# Resolve KB to an absolute path so it remains valid after cd into FAKE_REPO.
_KB_DEFAULT=$(cd "$(dirname "${BATS_TEST_FILENAME}")/.." && pwd)/result/bin/kb
KB=${KB:-$_KB_DEFAULT}

setup() {
    # Create a fake repo with the .state/agent-kb/ layout that Paths::discover() expects.
    FAKE_REPO=$(mktemp -d /tmp/kb-peers-test-XXXXXX)
    mkdir -p "$FAKE_REPO/.state/agent-kb"
    # Disable embeddings — no model download needed.
    export KB_NO_EMBED=1
    # Ensure git does not report a repo root for the temp dir so detect_source_repo
    # falls back to walking up from the DB path (gives a stable source_repo value).
    export GIT_DIR=/dev/null
    export GIT_CEILING_DIRECTORIES="$FAKE_REPO"
}

teardown() {
    rm -rf "$FAKE_REPO"
}

# ---------------------------------------------------------------------------
# Helper: run kb from inside the fake repo so Paths::discover() finds .state/
# ---------------------------------------------------------------------------
kb() {
    (cd "$FAKE_REPO" && "$KB" "$@")
}

# ---------------------------------------------------------------------------
# 1. peers add: creates a peer and prints a UUID
# ---------------------------------------------------------------------------
@test "peers add: creates a peer and prints UUID" {
    run kb peers add /tmp/fake-repo --type epic --epic-slug test-slug
    [ "$status" -eq 0 ]
    # UUID format: 8-4-4-4-12 hex chars separated by hyphens (total 36 chars)
    [[ "$output" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]]
}

# ---------------------------------------------------------------------------
# 2. peers list: shows added peer in JSON
# ---------------------------------------------------------------------------
@test "peers list: shows added peer in JSON" {
    # Add a peer first.
    kb peers add /tmp/fake-repo --type epic --epic-slug test-slug

    run kb peers list
    [ "$status" -eq 0 ]

    # Output must be valid JSON.
    echo "$output" | jq . > /dev/null

    # Must contain our target_repo.
    result=$(echo "$output" | jq -r '.[] | select(.target_repo == "/tmp/fake-repo") | .target_repo')
    [ "$result" = "/tmp/fake-repo" ]
}

# ---------------------------------------------------------------------------
# 3. peers remove: idempotent (exit 0 even if the ID does not exist)
# ---------------------------------------------------------------------------
@test "peers remove: idempotent (exit 0 even if missing)" {
    # Initialise the DB by running any harmless command first.
    kb peers list > /dev/null

    run kb peers remove "00000000-0000-0000-0000-000000000000"
    [ "$status" -eq 0 ]
}

# ---------------------------------------------------------------------------
# 4. peers show: returns edges for a repo path
# ---------------------------------------------------------------------------
@test "peers show: returns edges for repo path" {
    kb peers add /tmp/fake-repo --type dep

    run kb peers show /tmp/fake-repo
    [ "$status" -eq 0 ]

    # Output must be valid JSON array with at least 1 element.
    echo "$output" | jq . > /dev/null
    count=$(echo "$output" | jq 'length')
    [ "$count" -ge 1 ]
}

# ---------------------------------------------------------------------------
# 5. peers import: idempotent seed import
# ---------------------------------------------------------------------------
@test "peers import: idempotent seed import" {
    SEED_FILE=$(mktemp /tmp/kb-peers-seed-XXXXXX.json)
    # source_repo must match $FAKE_REPO so 'kb peers list' (which filters by current repo) finds it.
    printf '[{"source_repo":"%s","target_repo":"/tmp/kb-import-target","graph_type":"dep"}]\n' \
        "$FAKE_REPO" > "$SEED_FILE"

    # First import — should insert 1 row and print "1".
    run kb peers import "$SEED_FILE"
    [ "$status" -eq 0 ]
    [ "$output" = "1" ]

    # Second import of identical file — stamp hit, prints "0", exit 0.
    run kb peers import "$SEED_FILE"
    [ "$status" -eq 0 ]
    [ "$output" = "0" ]

    # Verify exactly 1 dep-type peer via show on the target (source-agnostic).
    run kb peers show /tmp/kb-import-target
    [ "$status" -eq 0 ]
    echo "$output" | jq . > /dev/null
    count=$(echo "$output" | jq '[.[] | select(.graph_type == "dep")] | length')
    [ "$count" -eq 1 ]

    rm -f "$SEED_FILE"
}

# ---------------------------------------------------------------------------
# 6. peers edge cleanup-epic: removes all edges for an epic slug
# ---------------------------------------------------------------------------
@test "peers edge cleanup-epic: removes all edges for slug" {
    kb peers add /tmp/repo-a --type epic --epic-slug test
    kb peers add /tmp/repo-b --type epic --epic-slug test

    run kb peers edge cleanup-epic test
    [ "$status" -eq 0 ]

    run kb peers list
    [ "$status" -eq 0 ]
    count=$(echo "$output" | jq '[.[] | select(.epic_slug == "test")] | length')
    [ "$count" -eq 0 ]
}

# ---------------------------------------------------------------------------
# 7. peers edge add: creates a directed edge between two repo paths
# ---------------------------------------------------------------------------
@test "peers edge add: creates a directed edge" {
    run kb peers edge add /tmp/edge-src /tmp/edge-tgt --type dep
    [ "$status" -eq 0 ]
    # Returns a UUID for the new peer entry.
    [[ "$output" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]]
}

# ---------------------------------------------------------------------------
# 8. peers edge list: lists all edges (optionally by epic slug)
# ---------------------------------------------------------------------------
@test "peers edge list: lists edges" {
    kb peers edge add /tmp/edge-list-src /tmp/edge-list-tgt --type dep

    run kb peers edge list
    [ "$status" -eq 0 ]
    echo "$output" | jq . > /dev/null
    count=$(echo "$output" | jq 'length')
    [ "$count" -ge 1 ]
}

# ---------------------------------------------------------------------------
# 9. peers edge remove: removes an edge by UUID (idempotent)
# ---------------------------------------------------------------------------
@test "peers edge remove: removes edge idempotently" {
    EDGE_ID=$(kb peers edge add /tmp/edge-rm-src /tmp/edge-rm-tgt --type dep)

    run kb peers edge remove "$EDGE_ID"
    [ "$status" -eq 0 ]

    # Removing again (missing ID) must still exit 0.
    run kb peers edge remove "$EDGE_ID"
    [ "$status" -eq 0 ]
}
