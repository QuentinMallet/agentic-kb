#!/usr/bin/env python3
"""Combine Hyperfine JSON files into the stable interactive benchmark schema."""
import argparse, datetime, json, math, pathlib, subprocess

def percentiles(times):
    if not times: raise ValueError("times must not be empty")
    ordered=sorted(times)
    def nearest_rank(p): return ordered[max(0, math.ceil(p*len(ordered))-1)]
    return {"p50_seconds":nearest_rank(.50), "p95_seconds":nearest_rank(.95), "max_seconds":ordered[-1]}

def main():
    p=argparse.ArgumentParser(); p.add_argument("--raw-dir",required=True); p.add_argument("--output",required=True); p.add_argument("--mode",required=True); p.add_argument("--elapsed-seconds",type=float,required=True); p.add_argument("--sample-size",type=int,required=True); p.add_argument("--large-sample-size",type=int,required=True); p.add_argument("--sample-size-map"); p.add_argument("--embedder",default="BenchEmbedder deterministic 384-dim pool"); p.add_argument("--artifact-note",default="CLI end-to-end; p95<500ms@10k cold is authoritative. 100k uses reduced sample_size; Criterion 100k groups use SamplingMode::Flat."); p.add_argument("--seed",type=int,default=42); a=p.parse_args()
    raw=pathlib.Path(a.raw_dir); results={}
    for path in sorted(raw.glob("*.json")):
        data=json.loads(path.read_text())
        for result in data["results"]: results[path.stem]={**percentiles(result["times"]),"times_seconds":result["times"]}
    sample_size_map=json.loads(a.sample_size_map) if a.sample_size_map else {"10000":a.sample_size,"100000":a.large_sample_size}
    def git(*args):
        try:return subprocess.check_output(["git",*args],text=True).strip()
        except Exception:return "unknown"
    artifact={"meta":{"date":datetime.date.today().isoformat(),"branch":git("branch","--show-current"),"commit":git("rev-parse","--short","HEAD"),"embedder":a.embedder,"seed":a.seed,"sample_size":sample_size_map,"note":a.artifact_note,"mode":a.mode,"actual_elapsed_seconds":a.elapsed_seconds},"results":results}
    pathlib.Path(a.output).write_text(json.dumps(artifact,indent=2)+"\n")

if __name__=="__main__": main()
