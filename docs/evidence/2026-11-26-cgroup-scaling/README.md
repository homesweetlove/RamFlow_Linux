# Cgroup-aware scheduling scalability study (2026-11-26)

Research results for RamFlow 0.2.0, tested revision [`b36c4321fe85ad47d2bf3038e86216ea62a8ff04`](https://github.com/homesweetlove/RamFlow_Linux/tree/b36c4321fe85ad47d2bf3038e86216ea62a8ff04) ([research PR #1](https://github.com/homesweetlove/RamFlow_Linux/pull/1)). This report documents the research revision, not necessarily the current default-branch implementation.

## Protocol

- One Hyper-V Ubuntu VM, 2/4/6/8 vCPUs, Linux 6.8.0-151-generic, Chrome 158.0.8127.88, Node v22.23.5.
- Two cgroup v2 contexts: a non-root user-session CPU cgroup and root cgroup. Chrome and background workers share the specified cgroup, with background workers in separate sessions.
- Synthetic SHA-256 load: 4 background processes per vCPU, 2 threads per process, 64 KiB hash input. Thus 8/16/24/32 processes respectively.
- Four arms: `off` (untreated), `auto_group` (cgroup guard disabled), `auto_task` (autogroup disabled), `auto` (environment-aware scope selection). All active arms otherwise retain the adaptive controller.
- 20 randomized paired blocks per environment, 30 measurements of each Chrome search/sort operation per arm per block. Seeds: 20261118 (non-root), 20261125 (root). Total: 8 environments, 160 blocks, 640 arm executions, 38,400 operation measurements. Operations are not independent statistical replicates.
- p95 uses nearest-rank within each trial (29th ordered value out of 30). Reported reductions are ratios of medians of the 20 block p95 values, relative to `off`.

## Results

| vCPU | cgroup | Search reduction (95% CI) | Sort reduction (95% CI) | Background throughput change |
|---:|---|---:|---:|---:|
| 2 | non-root | 84.0% [83.1, 86.1] | 85.3% [82.8, 86.4] | -7.3% |
| 2 | root | 86.7% [85.3, 88.5] | 86.7% [85.2, 87.3] | -9.6% |
| 4 | non-root | 83.7% [82.9, 85.5] | 83.6% [82.6, 84.3] | -4.0% |
| 4 | root | 86.7% [83.0, 88.7] | 86.4% [84.3, 88.7] | -5.2% |
| 6 | non-root | 84.3% [83.6, 84.8] | 83.5% [82.5, 84.2] | -1.3% |
| 6 | root | 85.3% [84.0, 86.1] | 84.5% [83.2, 85.5] | -3.9% |
| 8 | non-root | 84.8% [84.2, 85.6] | 83.6% [83.0, 84.4] | -1.3% |
| 8 | root | 86.5% [84.4, 88.0] | 86.3% [84.6, 88.7] | -0.8% |

Paired percentile bootstrap CIs: resample 20 block indices with replacement, 50,000 replicates, NumPy default_rng(20261126), independently reset for each environment/metric; compute 100 × (1 − median(auto)/median(off)) per replicate, take 2.5th and 97.5th percentiles. Exact two-sided Wilcoxon signed-rank tests for auto versus each other arm, separately by metric/environment: all 32 contrasts versus off or the environment-inappropriate control have raw p = 2/2^20 = 0.00000190735 (Holm-adjusted across 48 comparisons: 0.0000915527). The 16 contrasts with the appropriate forced-scope control are not significant; this does not establish equivalence.

## Integrity and mechanism

All 640 raw trial records have empty error lists. All 480 active trials report restoration and journal removal. All 1,280 trial-level nearest-rank p95 values match the CSV and summary arrays exactly. No nonfinite or negative operation latencies were found. No observations were excluded.

| Active arm | Non-root withdrawals / executions | Root withdrawals / executions |
|---|---:|---:|
| auto_group | 80/80 | 0/80 |
| auto_task | 0/80 | 80/80 |
| auto | 0/80 | 0/80 |

The pattern supports the hypothesis that the appropriate control scope depends on CPU cgroup context. It is not evidence that RamFlow outperforms every external scheduler.

## Raw archive

The original archive is named `ramflow-full-study.tar.gz` (984 regular files, 16 directories), with eight `results/main-study-*` directories containing `raw.json`, `trials.csv`, `summary.json`, and 480 pairs of daemon logs and traces in total.

SHA-256 of the original archive:
`558d9274cb3d3628688eb8ff37ea86f80b28a5a9428340c9701b93ddbe41092d`

The binary archive is to be committed separately; this report alone is not the raw dataset. Retain synthetic process labels, PIDs, cgroup paths and measurement values for reproducibility. The archive was screened for obvious home paths, IP addresses, email addresses, VM hostname and credential keywords; none were found. This is not a guarantee of exhaustive anonymization.

## Reproduction

Use the benchmark at the tested revision, in a disposable VM only. Build the release binary, enable scheduler statistics and autogroup, ensure Node/Chrome are available, then run `bench/chrome-study/study.py` with `--blocks 20 --arms off,auto_group,auto_task,auto --bg <4*vCPU> --threads 2` and a fresh output directory. Use the appropriate seed and verify the actual cgroup membership of the harness, Chrome and workers. For root experiments the dedicated benchmark shell was moved into the root cgroup before launching the harness; do not move unrelated processes. This workload scales with CPU count and is not a fixed-load speedup experiment.

## Limitations

Single host, synthetic load and a headless Chrome dashboard; no independent hardware replication, fixed-load sweep, external scheduler baseline or general desktop workload validation yet. Block-level CIs characterize within-environment variability, not uncertainty across physical machines. Different contexts use different seeds. Prioritize external baselines and real-workload replication before claiming general performance or novelty.
