#!/usr/bin/env bash
# Copy .omc/ planning artifacts into the mdBook source tree.
#
# Usage: generate-planning-artifacts.sh <omc-dir> <build-docs-src-dir>
#   $1 = .omc/ source directory
#   $2 = build-docs/src/ output directory (the mdBook source root)

set -euo pipefail

OMC_DIR="${1}"
OUT_DIR="${2}"

ARTIFACTS_DIR="$OUT_DIR/18-planning-artifacts"

# Create output directories
mkdir -p "$ARTIFACTS_DIR/plans" "$ARTIFACTS_DIR/specs" "$ARTIFACTS_DIR/research"

# Track which sections have files
plans_files=()
specs_files=()
research_files=()

# Copy each subdirectory if it exists and has .md files
for subdir in plans specs research; do
    src="$OMC_DIR/$subdir"
    dst="$ARTIFACTS_DIR/$subdir"

    if [ ! -d "$src" ]; then
        continue
    fi

    while IFS= read -r -d '' f; do
        fname=$(basename "$f")
        cp "$f" "$dst/$fname"
        case "$subdir" in
            plans)    plans_files+=("$fname") ;;
            specs)    specs_files+=("$fname") ;;
            research) research_files+=("$fname") ;;
        esac
    done < <(find "$src" -maxdepth 1 -name "*.md" -print0 | sort -z)
done

# Determine if we found any files at all
total=$(( ${#plans_files[@]} + ${#specs_files[@]} + ${#research_files[@]} ))
if [ "$total" -eq 0 ]; then
    # Nothing to publish — leave SUMMARY.md untouched
    exit 0
fi

# Write README.md
cat > "$ARTIFACTS_DIR/README.md" <<'EOF'
# Planning Artifacts

Active planning artifacts for in-progress work. These are transitional documents —
plans, specs, and research notes that will be distilled into documentation pages
and then pruned when their parent epic closes.

- **Plans** (`.omc/plans/`): Implementation plans from ralplan consensus
- **Specs** (`.omc/specs/`): Requirements specs from deep-interview sessions
- **Research** (`.omc/research/`): Research notes for in-progress investigations
EOF

# Update SUMMARY.md idempotently:
# If "# Planning Artifacts" already exists, remove that heading and everything
# after it, then re-append. Otherwise just append.
SUMMARY="$OUT_DIR/SUMMARY.md"

if grep -q "^# Planning Artifacts" "$SUMMARY"; then
    # Truncate at the Planning Artifacts heading
    line_num=$(grep -n "^# Planning Artifacts" "$SUMMARY" | head -1 | cut -d: -f1)
    head -n $(( line_num - 1 )) "$SUMMARY" > "$SUMMARY.tmp"
    mv "$SUMMARY.tmp" "$SUMMARY"
fi

# Append the Planning Artifacts section
# Note: printf '%s\n' used for lines starting with '-' to avoid printf treating
# them as option flags on some bash implementations.
{
    printf '\n# Planning Artifacts\n\n'
    printf '%s\n' '- [Overview](18-planning-artifacts/README.md)'

    if [ ${#plans_files[@]} -gt 0 ]; then
        printf '%s\n' '  - [Plans](18-planning-artifacts/plans/)'
        for f in "${plans_files[@]}"; do
            name="${f%.md}"
            printf '    - [%s](18-planning-artifacts/plans/%s)\n' "$name" "$f"
        done
    fi

    if [ ${#specs_files[@]} -gt 0 ]; then
        printf '%s\n' '  - [Specs](18-planning-artifacts/specs/)'
        for f in "${specs_files[@]}"; do
            name="${f%.md}"
            printf '    - [%s](18-planning-artifacts/specs/%s)\n' "$name" "$f"
        done
    fi

    if [ ${#research_files[@]} -gt 0 ]; then
        printf '%s\n' '  - [Research](18-planning-artifacts/research/)'
        for f in "${research_files[@]}"; do
            name="${f%.md}"
            printf '    - [%s](18-planning-artifacts/research/%s)\n' "$name" "$f"
        done
    fi
} >> "$SUMMARY"
