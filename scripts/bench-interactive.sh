#!/usr/bin/env bash
set -euo pipefail
mode="${1:-cold}"; case "$mode" in cold|warm) ;; *) echo "usage: $0 [cold|warm]" >&2; exit 2;; esac
runs="${BENCH_RUNS:-20}"; large_runs="${BENCH_LARGE_RUNS:-10}"
root="$(git rev-parse --show-toplevel)"; stamp="$(date +%F)"; work="${BENCH_WORK_DIR:-$root/.state/bench-interactive}"; raw="$work/raw-$stamp-$mode"
reject_unsafe_path() {
  local label="$1" path="$2"
  if [[ "$path" == *"'"* || "$path" == *'"'* || "$path" == *\\* || "$path" == *$'\n'* || "$path" == *\`* ]]; then
    echo "Unsafe $label path (contains one of: ', \", \\, newline, or \`): $path" >&2
    exit 1
  fi
}
reject_unsafe_path "BENCH_WORK_DIR" "$work"
mkdir -p "$raw" "$root/.omc/benches"
model_cache="$root/.fastembed-cache"
if [[ ! -d "$model_cache" ]]; then model_cache="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")/.fastembed-cache"; fi
if [[ ! -d "$model_cache" ]]; then echo "Missing repository model cache: $model_cache" >&2; exit 1; fi
export FASTEMBED_CACHE_PATH="$model_cache"
echo "Estimated runtime: 10k ~8 minutes, 100k ~20 minutes, plus one model load + query embedding per hybrid run (mode=$mode, runs=$runs/$large_runs; warmups add 3 loads per size; fixture build time varies by disk)."
echo "Hybrid variant: query embedding + model-load cost is real; corpus embeddings are deterministic synthetic BenchEmbedder vectors. Model cache: $FASTEMBED_CACHE_PATH"
start="$(date +%s)"
cargo build --release --bin kb --bin kb-bench-fixture
kb="$root/target/release/kb"; builder="$root/target/release/kb-bench-fixture"; warm=(); [[ "$mode" == warm ]] && warm=(--warmup 3)
reject_unsafe_path "kb binary" "$kb"
printf -v kb_q '%q' "$kb"
for size in 10000 100000; do
  fixture="$work/fixture-$size-$mode"; if [[ -e "$fixture" ]]; then echo "Refusing to overwrite existing fixture: $fixture" >&2; exit 1; fi
  "$builder" "$fixture" "$size" 42
  count="$runs"; [[ "$size" == 100000 ]] && count="$large_runs"; config="$fixture/kb.toml"
  reject_unsafe_path "fixture" "$fixture"
  reject_unsafe_path "config" "$config"
  printf -v fixture_q '%q' "$fixture"
  printf -v config_q '%q' "$config"
  run_hf() { local name="$1" prep="$2" command="$3"; hyperfine --runs "$count" "${warm[@]}" --prepare "$prep" --export-json "$raw/${size}-${name}.json" "$command"; }
  run_hf search-verify-on "printf 'inline_verify_k = 10\\n[embed]\\nenabled = false\\n' > $config_q" "cd $fixture_q && KB_NO_EMBED=1 $kb_q search --fts 'architecture latency' >/dev/null"
  run_hf search-verify-off "printf 'inline_verify_k = 0\\n[embed]\\nenabled = false\\n' > $config_q" "cd $fixture_q && KB_NO_EMBED=1 $kb_q search --fts 'architecture latency' >/dev/null"
  run_hf context "true" "cd $fixture_q && KB_NO_EMBED=1 $kb_q context --budget 1000 >/dev/null"
  run_hf cited-by "true" "cd $fixture_q && KB_NO_EMBED=1 $kb_q cited-by src/hot.rs >/dev/null"
  run_hf search-hybrid-embed "printf 'inline_verify_k = 10\\n[embed]\\nenabled = true\\n' > $config_q" "cd $fixture_q && $kb_q search 'architecture latency' --limit 5 --content >/dev/null"
done
elapsed="$(( $(date +%s) - start ))"
python3 "$root/scripts/bench-percentiles.py" --raw-dir "$raw" --output "$root/.omc/benches/$stamp-interactive-cli-$mode.json" --mode "$mode" --elapsed-seconds "$elapsed" --sample-size "$runs" --large-sample-size "$large_runs"
echo "Artifact: .omc/benches/$stamp-interactive-cli-$mode.json (elapsed ${elapsed}s)"
