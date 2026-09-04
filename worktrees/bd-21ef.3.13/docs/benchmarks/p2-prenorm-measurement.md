# P2 pre-normalization measurement

The threshold and its marginal-cost basis were pre-registered in
`docs/decisions/p2-prenorm-threshold.md`. Do not revise them after collecting numbers.

Enter the Criterion point estimate for each variant and compute
`(cosine - dot) / cosine * 100`. Keep every site/corpus cell explicit, following the
`bd-tx0` `verify_matrix` methodology cited by the C3 plan.

| similarity site | corpus | cosine recomputing norm_b | pre-normalized dot | norm_b cost % |
|---|---:|---:|---:|---:|
| semantic | 1,000 | TODO | TODO | TODO |
| semantic | 10,000 | TODO | TODO | TODO |
| cue | 1,000 | TODO | TODO | TODO |
| cue | 10,000 | TODO | TODO | TODO |
| MMR (`limit=10`, pool=20) | 1,000 | TODO | TODO | TODO |
| MMR (`limit=10`, pool=20) | 10,000 | TODO | TODO | TODO |

Run from the repository devShell:

```bash
KB_NO_EMBED=1 cargo bench --bench norm_cost
```

If entering the devShell explicitly is required:

```bash
nix develop -c sh -c 'KB_NO_EMBED=1 cargo bench --bench norm_cost'
```
