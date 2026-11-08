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

## Roadmap (not yet)

* per-test-function isolation (`cargo test -- --list` discovery, each fn own process)
* `--partition slice:m/n|hash:m/n` sharding + `--rerun` from `result.json`
* `--filterset` + `fuzz-next.toml` per-test overrides (`threads-required`, `retries` per test)
* `ferry` plugin: `ferry run` checks budget before spawning agents (thin hook; core broker stays standalone)

## Testing

26 unit tests (budget admission/queue/release/replace, timing stats +
scaling + JSONL roundtrip, timeout/slow/terminate, retries→flaky, junit
escaping, justification/seen-estimate gates, percentile/backoff/duration
parsers). Integration tests run the gates against `sh -c` fakes so CI stays
fast without a full `cargo test` build.
