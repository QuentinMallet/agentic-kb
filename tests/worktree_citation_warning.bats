#!/usr/bin/env bats
# Regression smoke test for the kb_add worktree-citation warning.
# Run after nix build with: bats tests/worktree_citation_warning.bats

_KB_DEFAULT=$(cd "$(dirname "${BATS_TEST_FILENAME}")/.." && pwd)/result/bin/kb
KB=${KB:-$_KB_DEFAULT}

setup() {
    FAKE_REPO=$(mktemp -d /tmp/kb-worktree-warning-XXXXXX)
    mkdir -p "$FAKE_REPO/.state/agent-kb"
    mkdir -p "$FAKE_REPO/.state/worktrees/feature/src"
    printf 'fn warning_fixture() {}\n' > "$FAKE_REPO/.state/worktrees/feature/src/lib.rs"
    export KB_NO_EMBED=1
    export GIT_DIR=/dev/null
    export GIT_CEILING_DIRECTORIES="$FAKE_REPO"
}

teardown() {
    rm -rf "$FAKE_REPO"
}

kb() {
    (cd "$FAKE_REPO" && "$KB" "$@")
}

@test "kb add warns when citation path is in a disposable worktree" {
    run kb add --path test/warning --summary warning --content body \
        --tags test --kind convention \
        --evidence '{"kind":"code","citation_path":".state/worktrees/feature/src/lib.rs:1-2","citation_hash":"sha256:abc"}'

    [ "$status" -eq 0 ]
    [[ "$stderr" == *"warn: citation_path under .state/worktrees/ will go stale after the worktree is removed"* ]]
}
