#!/usr/bin/env bash
set -euo pipefail
mode="${1:-cold}"; case "$mode" in cold|warm) ;; *) echo "usage: $0 [cold|warm]" >&2; exit 2;; esac
runs="${BENCH_RUNS:-20}"; large_runs="${BENCH_LARGE_RUNS:-10}"
root="$(git rev-parse --show-toplevel)"; stamp="$(date +%F)"; work="${BENCH_WORK_DIR:-$root/.state/bench-interactive}"; raw="$work/raw-$stamp-$mode"
mkdir -p "$raw" "$root/.omc/benches"
echo "Estimated runtime: 10k ~8 minutes, 100k ~20 minutes (mode=$mode, runs=$runs/$large_runs; fixture build time varies by disk)."
start="$(date +%s)"
cargo build --release --bin kb --bin kb-bench-fixture
kb="$root/target/release/kb"; builder="$root/target/release/kb-bench-fixture"; warm=(); [[ "$mode" == warm ]] && warm=(--warmup 3)
for size in 10000 100000; do
  fixture="$work/fixture-$size-$mode"; if [[ -e "$fixture" ]]; then echo "Refusing to overwrite existing fixture: $fixture" >&2; exit 1; fi
  "$builder" "$fixture" "$size" 42
  count="$runs"; [[ "$size" == 100000 ]] && count="$large_runs"; config="$fixture/kb.toml"
  run_hf() { local name="$1" prep="$2" command="$3"; hyperfine --runs "$count" "${warm[@]}" --prepare "$prep" --export-json "$raw/${size}-${name}.json" "$command"; }
  run_hf search-verify-on "printf 'inline_verify_k = 10\\n[embed]\\nenabled = false\\n' > '$config'" "cd '$fixture' && KB_NO_EMBED=1 '$kb' search --fts 'architecture latency' >/dev/null"
  run_hf search-verify-off "printf 'inline_verify_k = 0\\n[embed]\\nenabled = false\\n' > '$config'" "cd '$fixture' && KB_NO_EMBED=1 '$kb' search --fts 'architecture latency' >/dev/null"
  run_hf context "true" "cd '$fixture' && KB_NO_EMBED=1 '$kb' context --budget 1000 >/dev/null"
  run_hf cited-by "true" "cd '$fixture' && KB_NO_EMBED=1 '$kb' cited-by src/hot.rs >/dev/null"
done
elapsed="$(( $(date +%s) - start ))"
python3 "$root/scripts/bench-percentiles.py" --raw-dir "$raw" --output "$root/.omc/benches/$stamp-interactive-cli-$mode.json" --mode "$mode" --elapsed-seconds "$elapsed" --sample-size "$runs" --large-sample-size "$large_runs"
echo "Artifact: .omc/benches/$stamp-interactive-cli-$mode.json (elapsed ${elapsed}s)"
