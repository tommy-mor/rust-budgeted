# cargo-fuzz-next design

## Why this exists

Joint-fuzz sweeps (`sweep.sh`: 13 `cargo test --release --test $t` binaries,
574s total for `post_match` alone) run in parallel Ferry worktrees with no
shared budget. On a 16-core Btrfs+LUKS box each worktree spawns dozens of
`rustc`/`cc1`/fuzz threads, the machine hits load 300, `kworker/events_unbound`
+ `btrfs-flush_delalloc` + `kcryptd` pin CPU, and Chrome stalls. The reboot
was a manual escape from that stall after a userspace OOM — Btrfs/DM-crypt
were the amplifier (CoW + zstd + LUKS + swap-on-Btrfs on 777 GiB of build
artefacts), not a filesystem bug.

## What it re-implements from nextest (and what it adds)

Nextest gives per-test isolation, timeouts, retries, JUnit, sharding, rerun.
We take the slice joint-fuzz actually needs and add three things nextest
knows nothing about:

1. **Global core budget** shared across worktrees/agents on the same box.
2. **Estimates from history** (timings.jsonl) with feasibility + ETA.
3. **Justification + seen-estimate gate** so agents pay for CPU with words.

## Budget broker

State (file-locked via fs2 `budget.lock`):

* `active.json`: `[{id, pid, test, cores, started_at, estimated_finish, worktree}]`
* `queue.json`: `[{id, test, cores, enqueued_at}]`
* `timings.jsonl`: history
* `runs/<id>/`: stdout/stderr/result/junit per run

Admission: `sum(active.cores) + request <= budget` (budget default `ncpus-4`).
Over → queue with ETA (poll 500ms), reject, or `--replace` (SIGTERM oldest
victim, grace 5s, SIGKILL). Dead pids reaped on every lock (unix `kill(pid,0)`).

No `taskset`/`systemd`/cgroup dependency (only Linux, brittle). Instead we
set child's internal pools via env (`CARGO_BUILD_JOBS`, `JOINT_FUZZ_THREADS`,
`RAYON_NUM_THREADS`, `TOKIO_WORKER_THREADS`, `NEXTEST_TEST_THREADS`) to the
granted cores. Cross-platform (Windows taskkill fallback).

## Estimator

* Store: append-only JSONL, corrupt lines skipped.
* Stats: p50/p95/p99 linear-interp percentiles.
* Cores scaling: `T(c) = T(ref) * (ref/c)^0.85` (sub-linear; joint-fuzz
  parallel ~7.4× on 16 cores).
* Cases scaling: linear when `--cases` given.
* Seed defaults from `sweep_summary.txt` (release, 8 threads) so the first
  runs already have sane estimates; defaults also trigger the approval gate
  for long tests.

## Executor

* Per-binary mode: `cargo test --release --test <name>` with thread-cap env.
* Captures stdout/stderr to `runs/<id>/` (never `| head` — panics survive).
* Poll 50ms; SLOW after `slow_timeout`; terminate after
  `slow_timeout * terminate_after` or `global_timeout` (process group SIGTERM,
  then SIGKILL after grace).
* Retries: fixed/exponential + jitter + max-delay; `TRY 2 PASS` → `flaky`.

## Agent wait primitive (`wait`/`poll` + `run --background`)

Agents launch 300s runs and need to block efficiently without writing
`while status; do sleep; done` loops over `status`/`runs`.

* `run --background` (`--detach`): validate the gates synchronously in the
  parent (failures still exit 2 immediately), reserve `runs/<id>/`
  atomically (`create_dir`, so two launches with the same `--run-id` race
  safely — loser exits 2), then re-exec self as
  `run --background-child --run-id <id> <original argv sans --background>`,
  `setsid` on unix, stdout/stderr → `runs/<id>/broker.log`. Parent prints
  only the id (or `{"run_id":"…"}` with `--json`) and exits 0. Original argv
  is reused verbatim so `--cargo-args`/`--` pass-through semantics survive.
* `wait` (alias `poll`) `<RUN_ID>…` / `--test <name>`: polls
  timings/active/queue every `--interval` (default 2s) until every target is
  Done (timing record exists) or Cancelled (seen active/queued then vanished
  with no record). Exit mirrors the run: 0 pass, 1 fail/cancelled, 2 unknown,
  124 wait-timeout. `--json` prints the final result as one object/array;
  `--verbose` prints `queued #N` / `active … remaining` ticks to stderr.
  `wait --test` discovers active/queued runs of the test *plus* timing
  records finished since (30s lookback) the wait began, so a short run that
  lives less than one poll interval is still captured via its record.
* `status --poll` blocks until active+queue are empty. Unknown ids exit 2
  after a 30s grace (typo'd ids do not hang forever); shorter `--timeout`
  wins.

## Roadmap (not yet)

* per-test-function isolation (`cargo test -- --list` discovery, each fn own process)
* `--partition slice:m/n|hash:m/n` sharding + `--rerun` from `result.json`
* `--filterset` + `fuzz-next.toml` per-test overrides (`threads-required`, `retries` per test)
* `ferry` plugin: `ferry run` checks budget before spawning agents (thin hook; core broker stays standalone)

## Testing

36 unit tests (budget admission/queue/release/replace/duplicate-guard,
timing stats + scaling + JSONL roundtrip, timeout/slow/terminate,
retries→flaky, junit escaping, justification/seen-estimate gates,
percentile/backoff/duration parsers, poll snapshots + wait aggregation +
idle). Integration tests run the gates against `sh -c` fakes plus CLI
parsing for `wait`/`poll`/`--background`/`--poll` so CI stays fast without
a full `cargo test` build.
