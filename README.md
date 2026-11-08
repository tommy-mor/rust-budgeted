# cargo-fuzz-next

Nextest-slice test runner for joint-fuzz style workspaces: **shared core + memory
budgets across worktrees, timing JSONL estimates (time + RAM), 50-word
justification gate, timeouts, retries, JUnit**.

It exists because `cargo test --release --test native_fuzz` in 5 Ferry
worktrees at once pins 16 cores, parks Chrome, and thrashes Btrfs/DM-crypt
writeback. This runner makes that cost visible *before* the run, allocates
cores + RAM cooperatively, and remembers every run — including *why* it ran.

## The three gates every `run` must pass

```
$ cargo fuzz-next estimate --test native_fuzz --cores 8
# → time 368s (p95), 2944 core-s  |  mem 1800 MiB (p95), 662400 mem-s

$ cargo fuzz-next run --test native_fuzz --cores 8 \
    --justification "<50+ words: why this deserves 2944 core-s + 662400 mem-s NOW>" \
    --hypothesis "<falsifiable expectation + decision it unlocks>" \
    --seen-estimate-secs 368 --seen-estimate-mem-mb 1800
# → admits / queues with ETA, runs, records, releases
```

1. **Justification + Hypothesis ≥ 50 words combined** — otherwise `run` is
   BLOCKED. The text is stored in `timings.jsonl` + `result.json` forever,
   so future you can remember why a 574s `post_match` was worth it.
2. **`--seen-estimate-secs` + `--seen-estimate-mem-mb` must match the store**
   (within tolerance) — proves the agent actually ran `estimate` instead
   of launching blind on *both* dimensions.
3. **Approval** — if `p95 > approval-threshold` (default `5m`) **or**
   `mem_p95 > mem-approval-threshold` (default `4096` MiB), `run` needs
   `--yes`. Without it the exit code is `2` with the estimate and template printed.

Bypass gates only with `--allow-no-justification` (CI / emergencies).

## Budget semantics (cross-worktree, cross-platform, cores + memory)

* Global budgets = `ncpus - reserved` (default reserved 4, so 12 on 16 cores)
  and `total-RAM - reserved` (default reserve ~50%, so ~8 GiB on 15 GiB).
  Override by `--budget-cores 12 --budget-mem-mb 8192`, env
  `FUZZ_NEXT_BUDGET_CORES` / `FUZZ_NEXT_BUDGET_MEM_MB`, or
  `fuzz-next.toml: budget-cores = 12` / `budget-mem-mb = 8192`.
* Admission is a 2-D sum: `sum(active.cores)+req.cores <= budget_cores` **and**
  `sum(active.mem)+req.mem <= budget_mem` → admit. Either dimension over
  → queue (default) with ETA, reject with `--no-queue`, or cancel the oldest
  victim with `--replace` (prefers one freeing enough of both).
* Every run's `--mem-mb` (default = estimate p95) is the admission request;
  the executor samples **peak RSS of the whole process tree** every 200ms
  (child + `rustc`/`cc1` grandchildren) into `peak_rss_mb` history.
* Queued runs print `starts in ~Xs, finishes in ~Ys` and poll until admitted.
* No `taskset`, no `systemd`, no cgroups. Cooperative capping only:
  the child gets `CARGO_BUILD_JOBS`, `JOINT_FUZZ_THREADS`,
  `RAYON_NUM_THREADS`, `TOKIO_WORKER_THREADS`, `NEXTEST_TEST_THREADS` all set
  to the granted cores. Btrfs still does the work, but you never launch more
  than budget cores of children at once — that is what stops the 300-load stall.
* State lives in `~/.local/share/fuzz-next/` (`active.json`, `queue.json`,
  `budget.lock`, `timings.jsonl`, `runs/`). Override with
  `--state-dir` / `$FUZZ_NEXT_STATE_DIR` (per-repo DBs, tests).

### Ferry integration (recommended: sidecar, not fork)

Ferry (`~/programming/clones/ferry`) has **no CPU/memory budgeting** — only
`flock` correctness locks, no core pool, no queue, no `MemoryMax`, no
`CARGO_BUILD_JOBS` caps. Keep the broker separate and wrap Ferry:

```bash
cargo fuzz-next run --test native_fuzz --cores 8 --mem-mb 1800 \
  --justification "..." --hypothesis "..." \
  --seen-estimate-secs 368 --seen-estimate-mem-mb 1800 --yes
# or, once the broker shares Ferry's store ($FERRY_HOME/broker/):
ferry run agent-a45 -- env CARGO_BUILD_JOBS=8 JOINT_FUZZ_THREADS=8 cargo test --release --test native_fuzz
```

The broker's state dir can point at `$FERRY_HOME/broker/` so `ferry run`
(which sets `CARGO_TARGET_DIR` per agent from cwd) and `cargo fuzz-next`
share one lease file. Do **not** put budget checks inside Ferry's shim hot
path — every cargo call would pay a lease round-trip, and `ferry.toml` is
already arbitrary commands. `ferry --json list` already exposes `busy`/`running`,
enough for `status` to join against.

## Timings + estimates

`timings.jsonl` (append-only, robust to corrupt lines):

```json
{"ts":"…","test":"native_fuzz","secs":307.0,"cores":8,"mem_mb_requested":1800,
 "peak_rss_mb":1500.0,"core_seconds":2456,"mem_seconds":522000,
 "rc":0,"status":"pass","justification":"…50 words…","hypothesis":"…",
 "seen_estimate_secs":368.0,"seen_estimate_mem_mb":1800.0,"run_id":"…"}
```

`estimate` returns time p50/p95/p99 scaled by cores (`^0.85`, matching
joint-fuzz ~7.4× on 16 cores) and cases, **plus** memory p50/p95 scaled by
cores (`^0.30` — more threads ≈ more stacks/arenas). Before history exists it
falls back to built-in joint-fuzz defaults (`post_match 574s/2500MiB`,
`native_fuzz 307s/1800MiB`, `rooms 12s/400MiB`, …).

Per-test stats: `cargo fuzz-next runs --last 50 --test native_fuzz --json`.

## Agent polling: `wait` / `poll` + `run --background`

Runs are long (minutes, 100s of core-s). Instead of hand-writing
`while status` loops, use the built-in wait primitive:

```bash
# Launch detached (prints only the run_id; gates still checked synchronously).
RUN_ID=$(cargo fuzz-next run --test native_fuzz --cores 8 --mem-mb 1800 \
  --justification "..." --hypothesis "..." \
  --seen-estimate-secs 368 --seen-estimate-mem-mb 1800 --yes --background)
echo "launched $RUN_ID"
# ... do other work ...

# Block until that run finishes. Exit mirrors the run: 0 pass, 1 fail/cancelled,
# 2 unknown id, 124 wait-timeout. Alias: `cargo fuzz-next poll <id>`.
cargo fuzz-next wait "$RUN_ID" --verbose          # progress to stderr
cargo fuzz-next poll "$RUN_ID" --json --timeout 15m  # machine output on stdout
cargo fuzz-next wait --test native_fuzz           # wait for any active/queued run of the test
cargo fuzz-next status --poll                     # block until the budget is idle
```

* `run --background` (`--detach` alias) validates the gates in the parent
  (gate failures exit `2` immediately, no orphan), then spawns a detached
  child (`setsid` on unix) whose stdout/stderr go to `runs/<id>/broker.log`.
  `--run-id ID` picks the id up-front (default is derived); the directory
  `runs/<id>/` is reserved atomically so two background launches with the
  same id cannot race. `--json` prints `{"run_id":"…"}` instead of the bare id.
* `wait`/`poll` takes one or more `RUN_ID`s (or `--test NAME`). It polls
  `timings.jsonl` + `active.json` + `queue.json` every `--interval` (default
  `2s`) for you. `--verbose` prints `queued #N …` / `active … remaining` ticks
  to stderr; final human line (or `--json` object/array) goes to stdout.
  A run that was queued/active and then vanishes with no timing record is
  reported as `cancelled`. An id that never appears exits `2` after a 30s
  unknown grace (shorter `--timeout` wins).
* `status --poll` blocks until no active and no queued runs remain
  (`--interval`, `--timeout`, `--json` supported).

## Nextest slice implemented

* per-binary mode (matches `sweep.sh`: `--test $t`)
* slow detection (`--slow-timeout 60s` → `SLOW` log)
* terminate-after + grace (`--slow-timeout 30s --terminate-after 4`, SIGTERM→SIGKILL, whole process group on unix)
* global timeout (`--global-timeout 10m`)
* retries + flaky marking (`--retries 2`, exponential backoff + jitter)
* JUnit XML (`--junit path`, plus `runs/<id>/junit.xml` always)
* fail tail + full logs (`runs/<id>/stdout.log`, `stderr.log`, `result.json` — never piped through `head`, panics survive)
* `estimate`, `status`, `runs`, `wait`/`poll`, `cancel`, `clean`, `doctor`
* `run --background`/`--detach` + `--run-id` + `wait`/`poll` + `status --poll` (agent wait primitive)

Not yet: per-test-function isolation (each `#[test]` in own process), sharding
`--partition`, filtersets. See `docs/design.md` for the roadmap.

## Justification template (≥50 words)

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
cargo fuzz-next run --test native_fuzz --cores 8 --mem-mb 1800 --release \
    --justification "..." --hypothesis "..." \
    --seen-estimate-secs 368 --seen-estimate-mem-mb 1800 --yes
# Agent async: launch detached, then block for the result.
RUN_ID=$(cargo fuzz-next run --test native_fuzz --cores 8 --background --run-id agent-1 \
    --justification "..." --hypothesis "..." \
    --seen-estimate-secs 368 --seen-estimate-mem-mb 1800 --yes)
cargo fuzz-next wait "$RUN_ID" --verbose   # alias: cargo fuzz-next poll "$RUN_ID"
cargo fuzz-next status --poll   # block until idle
cargo fuzz-next status
cargo fuzz-next runs --last 20
```

Config (`fuzz-next.toml` in cwd, subset of nextest.toml):

```toml
[profile.default]
budget-cores = 12
reserved-cores = 4
budget-mem-mb = 8192
reserved-mem-mb = 8192
approval-threshold = "5m"
mem-approval-threshold-mb = 4096
slow-timeout = "60s"
```

## Design

See `docs/design.md`.
