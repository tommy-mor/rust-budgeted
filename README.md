# cargo-fuzz-next

Nextest-slice test runner for joint-fuzz style workspaces: **shared core budget
across worktrees, timing JSONL estimates, 200-word justification gate,
timeouts, retries, JUnit**.

It exists because `cargo test --release --test native_fuzz` in 5 Ferry
worktrees at once pins 16 cores, parks Chrome, and thrashes Btrfs/DM-crypt
writeback. This runner makes that cost visible *before* the run, allocates
cores cooperatively, and remembers every run — including *why* it ran.

## The three gates every `run` must pass

```
$ cargo fuzz-next estimate --test native_fuzz --cores 8
# → effective 368s (p95), 2944 core-seconds

$ cargo fuzz-next run --test native_fuzz --cores 8 \
    --justification "<200+ words: why this deserves 2944 core-s NOW>" \
    --hypothesis "<falsifiable expectation + decision it unlocks>" \
    --seen-estimate-secs 368
# → admits / queues with ETA, runs, records, releases
```

1. **Justification + Hypothesis ≥ 200 words combined** — otherwise `run` is
   BLOCKED. The text is stored in `timings.jsonl` + `result.json` forever,
   so future you can remember why a 574s `post_match` was worth it.
2. **`--seen-estimate-secs` must match the store** (within tolerance) —
   proves the agent actually ran `estimate` instead of launching blind.
3. **Long-run approval** — if `p95 > approval-threshold` (default `5m`),
   `run` needs `--yes`. Without it the exit code is `2` with the estimate
   and template printed.

Bypass gates only with `--allow-no-justification` (CI / emergencies).

## Budget semantics (cross-worktree, cross-platform)

* Global budget = `ncpus - reserved` (default reserved 4, so 12 on 16 cores).
  Override by `--budget-cores 12`, `$FUZZ_NEXT_BUDGET_CORES`, or
  `fuzz-next.toml: budget-cores = 12`.
* Admission is a sum: `sum(active.cores) + request.cores <= budget` → admit.
  Otherwise: queue (default) with ETA, reject with `--no-queue`, or cancel the
  oldest victim with `--replace`.
* Queued runs print `starts in ~Xs, finishes in ~Ys` and poll until admitted.
* No `taskset`, no `systemd`, no cgroups. Cooperative capping only:
  the child gets `CARGO_BUILD_JOBS`, `JOINT_FUZZ_THREADS`,
  `RAYON_NUM_THREADS`, `TOKIO_WORKER_THREADS`, `NEXTEST_TEST_THREADS` all set
  to the granted cores. Btrfs still does the work, but you never launch more
  than budget cores of children at once — that is what stops the 300-load stall.
* State lives in `~/.local/share/fuzz-next/` (`active.json`, `queue.json`,
  `budget.lock`, `timings.jsonl`, `runs/`). Override with
  `--state-dir` / `$FUZZ_NEXT_STATE_DIR` (per-repo DBs, tests).

## Timings + estimates

`timings.jsonl` (append-only, robust to corrupt lines):

```json
{"ts":"…","test":"native_fuzz","secs":307.0,"cores":8,"core_seconds":2456,
 "rc":0,"status":"pass","justification":"…200 words…","hypothesis":"…",
 "seen_estimate_secs":368.0,"run_id":"…"}
```

`estimate` returns p50/p95/p99 scaled by cores (sub-linear `^0.85`, matching
joint-fuzz ~7.4× on 16 cores) and by cases. Before history exists it falls
back to built-in joint-fuzz defaults (`post_match 574s`, `native_fuzz 307s`,
`rooms 12s`, …).

Per-test stats: `cargo fuzz-next runs --last 50 --test native_fuzz --json`.

## Nextest slice implemented

* per-binary mode (matches `sweep.sh`: `--test $t`)
* slow detection (`--slow-timeout 60s` → `SLOW` log)
* terminate-after + grace (`--slow-timeout 30s --terminate-after 4`, SIGTERM→SIGKILL, whole process group on unix)
* global timeout (`--global-timeout 10m`)
* retries + flaky marking (`--retries 2`, exponential backoff + jitter)
* JUnit XML (`--junit path`, plus `runs/<id>/junit.xml` always)
* fail tail + full logs (`runs/<id>/stdout.log`, `stderr.log`, `result.json` — never piped through `head`, panics survive)
* `estimate`, `status`, `runs`, `cancel`, `clean`, `doctor`

Not yet: per-test-function isolation (each `#[test]` in own process), sharding
`--partition`, filtersets. See `docs/design.md` for the roadmap.

## Justification template (≥200 words)

1. What question does this run answer, and what cheaper alternative did we rule out?
2. Why is THIS test the right tool (invariant/proof/soak)?
3. Why NOW — what blocked us, what unblocks, what decision hinges on it?
4. Why these cores/cases — what does timing history say, what breaks at half?
5. Hypothesis + falsifier (pre-registered expectation + what refutes it)?
6. If it flakes/fails, what's the next step and cost of finding out late?
7. What queued runs wait for these cores (ETA impact)?

## Install / usage

```bash
cargo install --path .   # or: cargo build --release
cargo fuzz-next --help
cargo fuzz-next estimate --test native_fuzz --cores 8
cargo fuzz-next run --test native_fuzz --cores 8 --release \
    --justification "..." --hypothesis "..." --seen-estimate-secs 368 --yes
cargo fuzz-next status
cargo fuzz-next runs --last 20
```

Config (`fuzz-next.toml` in cwd, subset of nextest.toml):

```toml
[profile.default]
budget-cores = 12
reserved-cores = 4
approval-threshold = "5m"
slow-timeout = "60s"
```

## Design

See `docs/design.md`.
