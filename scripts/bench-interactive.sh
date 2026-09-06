#!/usr/bin/env bash
set -euo pipefail
mode="${1:-cold}"; case "$mode" in cold|warm) ;; *) echo "usage: $0 [cold|warm]" >&2; exit 2;; esac
runs="${BENCH_RUNS:-20}"; large_runs="${BENCH_LARGE_RUNS:-10}"; write_runs="${BENCH_WRITE_RUNS:-200}"; write_fixture_size="${BENCH_WRITE_FIXTURE_SIZE:-10000}"
lanes="${BENCH_LANES:-read,write}"
root="$(git rev-parse --show-toplevel)"; stamp="$(date +%F)"; work="${BENCH_WORK_DIR:-$root/.state/bench-interactive}"; raw="$work/raw-$stamp-$mode"
run_read=0; run_write=0
IFS=',' read -r -a lane_list <<< "$lanes"
for lane in "${lane_list[@]}"; do
  case "$lane" in
    read) run_read=1 ;;
    write) run_write=1 ;;
    "") ;;
    *) echo "Unknown BENCH_LANES entry: $lane" >&2; exit 2 ;;
  esac
done
if [[ "$run_read" -eq 0 && "$run_write" -eq 0 ]]; then
  echo "BENCH_LANES must enable at least one lane" >&2
  exit 2
fi
reject_unsafe_path() {
  local label="$1" path="$2"
  if [[ "$path" == *"'"* || "$path" == *'"'* || "$path" == *\\* || "$path" == *$'\n'* || "$path" == *\`* ]]; then
    echo "Unsafe $label path (contains one of: ', \", \\, newline, or \`): $path" >&2
    exit 1
  fi
}
reject_unsafe_path "BENCH_WORK_DIR" "$work"
mkdir -p "$raw" "$root/.omc/benches"
if [[ "$run_read" -eq 1 ]]; then
  model_cache="$root/.fastembed-cache"
  if [[ ! -d "$model_cache" ]]; then model_cache="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")/.fastembed-cache"; fi
  if [[ ! -d "$model_cache" ]]; then echo "Missing repository model cache: $model_cache" >&2; exit 1; fi
  export FASTEMBED_CACHE_PATH="$model_cache"
  echo "Estimated runtime: 10k ~8 minutes, 100k ~20 minutes, plus one model load + query embedding per hybrid run (mode=$mode, runs=$runs/$large_runs; warmups add 3 loads per size; fixture build time varies by disk)."
  echo "Hybrid variant: query embedding + model-load cost is real; corpus embeddings are deterministic synthetic BenchEmbedder vectors. Model cache: $FASTEMBED_CACHE_PATH"
fi
start="$(date +%s)"
cargo build --release --bin kb --bin kb-bench-fixture
target_dir="${CARGO_TARGET_DIR:-$root/target}"
kb="$target_dir/release/kb"; builder="$target_dir/release/kb-bench-fixture"; warm=(); [[ "$mode" == warm ]] && warm=(--warmup 3)
reject_unsafe_path "kb binary" "$kb"
printf -v kb_q '%q' "$kb"
if [[ "$run_read" -eq 1 ]]; then
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
fi

if [[ "$run_write" -eq 1 ]]; then
  write_raw="$work/raw-$stamp-write-$mode"
  write_base="$work/write-base-$write_fixture_size-$mode"
  write_run="$work/write-run-$write_fixture_size-$mode"
  mkdir -p "$write_raw"
  if [[ -e "$write_base" ]]; then echo "Refusing to overwrite existing fixture: $write_base" >&2; exit 1; fi
  "$builder" "$write_base" "$write_fixture_size" 42 >/dev/null
  reject_unsafe_path "write fixture base" "$write_base"
  reject_unsafe_path "write fixture run" "$write_run"
  printf -v write_base_q '%q' "$write_base"
  printf -v write_run_q '%q' "$write_run"
  write_start="$(date +%s)"
  # Copy the whole fixture repository: the database and its event log/cursor
  # sidecars are one converged unit and must never be reset independently.
  hyperfine --runs "$write_runs" "${warm[@]}" \
    --prepare "rm -rf $write_run_q && mkdir -p $write_run_q && cp -R $write_base_q/. $write_run_q" \
    --export-json "$write_raw/${write_fixture_size}-add-write.json" \
    "cd $write_run_q && KB_NO_EMBED=1 $kb_q add --id bench-write-lane --path bench/write/added-entry --summary 'bench write lane' --content 'write path benchmark entry for log append and db apply' --tags bench,write --version-ref bench-write >/dev/null"
  write_elapsed="$(( $(date +%s) - write_start ))"
  python3 "$root/scripts/bench-percentiles.py" \
    --raw-dir "$write_raw" \
    --output "$root/.omc/benches/$stamp-interactive-cli-write-$mode.json" \
    --mode "$mode" \
    --elapsed-seconds "$write_elapsed" \
    --sample-size "$write_runs" \
    --large-sample-size "$write_runs" \
    --sample-size-map "{\"$write_fixture_size\": $write_runs}" \
    --embedder "KB_NO_EMBED=1 (NoopEmbedder CLI lane)" \
    --artifact-note "CLI end-to-end write path; each hyperfine sample runs one kb add on a fresh temp copy of the seeded fixture so p50/p95 are per-add latencies from a fixed starting state." \
    --seed 42
  echo "Artifact: .omc/benches/$stamp-interactive-cli-write-$mode.json (elapsed ${write_elapsed}s)"
fi
