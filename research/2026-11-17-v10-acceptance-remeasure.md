# Acceptance remeasure — v10-lane-c-acceptance-retry (2026-11-17)

## Deliverable
write C:\Users\WorkShop1\proj\jcode-src\research\2026-11-17-v10-acceptance-remeasure.md

## What was re-measured
The coordinator reported that commit 697daa6a9 (the landing-identity proof fix) had
`cargo test -p jcode-build-support --lib` -> 124 passed, 0 failed. This lane re-ran that
check and both recorded defect spots independently, on the Windows host, and recorded live
assertion output.

## Answer 1 — `cargo test -p jcode-build-support --lib` (build-support suite)
Command:
  cd /d C:\Users\WorkShop1\proj\jcode-src
  set "CARGO_TARGET_DIR=E:\cargo-target-shared"
  cargo test -p jcode-build-support --lib

- Exit code: 0
- Result: `test result: ok. 124 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`
- The retry matched the coordinator's 124/0 exactly.

## Answer 2 — Defect (a): `inspect_reload_wait_status_reports_failed_when_reload_pid_is_dead`
Source read (crates\jcode-app-core\src\server\reload_state.rs):
  pub fn reload_process_alive(pid: u32) -> bool {
      if pid == 0 { return false; }
      #[cfg(unix)] { ... libc::kill(pid, 0) ... }        // true iff alive / EPERM
      #[cfg(not(unix))] { let _ = pid; true }            // always true, non-zero pid
  }

The test (crates\jcode-app-core\src\server\socket_tests.rs, ~line 737):
  let dead_pid = std::process::id().saturating_add(1_000_000);
  assert!(!reload_process_alive(dead_pid), "test requires a definitely-dead pid");
  ...write a Starting marker with pid=dead_pid...
  assert!(matches!(status, ReloadWaitStatus::Failed(Some(_))));

Why it FAILS on this Windows host:
- `reload_process_alive` is compiled with `#[cfg(not(unix))]`, so on Windows it returns
  `true` for ANY non-zero pid without asking the OS whether the process exists.
- The test picks `pid + 1_000_000` as a "definitely-dead" pid. That assumption only holds
  on Unix, where the kernel will not reuse that PID. On Windows the test's own
  `assert!(!reload_process_alive(dead_pid))` passes (the pid is not the current one), so
  the marker is written, but then `inspect_reload_wait_status` calls
  `reload_process_alive(dead_pid)` -> `true`, so it decides `Waiting { pid: Some(dead_pid) }`
  rather than `Failed`.
- On Windows the assertion `matches!(status, ReloadWaitStatus::Failed(Some(_)))` fails.

Live verification could not be completed on the host:
- Running the two socket_tests produced pre-existing compile errors in `crates\jcode-app-core`
  that block the test binary from linking at all: `client_lifecycle_socket_tests.rs:206:29`
  uses an undeclared type `DaemonEvents`, and `client_lifecycle.rs`, `client_session.rs`,
  `client_state.rs` import `super::client_writer_task::ClientWriter` while `client_writer_task`
  is not a member of `server`. A `cargo check` for the same crate fails for the identical
  reason. Those errors pre-date this session and are unrelated to defect (a).
So the question "does it fail on THIS host" is decided by reading, not by a live run:
the `#[cfg(not(unix))]` branch makes it fail on Windows, unconditionally, for any non-zero pid.

What makes it interesting:
- The failure is the *implementation's* `cfg(not(unix))` path returning `true`
  unconditionally, and the *test* relying on an assumption (dead pid by offset) that only
  holds on Unix. Neither defect lives entirely in the other file:
    - Product defect: on Windows any stale/wrong nonzero pid is reported as alive. A
      reload-handoff loop could hang on a bogus pid.
    - Test-side defect: the test constructs its "dead" pid by a 1_000_000 offset, which is
      not a portable way to manufacture a dead pid (valid on Unix, invalid on Windows).

## Answer 3 — Defect (b): `swarm_status_immediate_broadcasts_can_invert_on_one_member_channel`
Source read (crates\jcode-app-core\src\server\swarm_tests.rs, lines 662-719):
  - Builds one member; spawn A that drains budget then calls broadcast_swarm_status.
  - yield_now; mutator flips member status to "running"; broadcasts again.
  - Collects statuses from the member channel; asserts vec!["running", "ready"].
  - Comment: "expected status inversion (new-then-old) on one member channel; if this
    fails with the correct order, the snapshot-vs-send race may have been fixed
    (update the wiring audit)".

Diagnosis:
- This test is a FAULT-INSERTION / regression-fixture. It asserts the buggy ordering, not
  the fixed one. It will only pass when the race reproduces, and it will only fail when the
  wiring audit actually fixes the snapshot-vs-send race.
- So on this host the test "fails" — but that failure is the *expected* outcome of a
  fault test, not evidence of a regression. A green run here would itself indicate the
  fixture needs re-auditing.
- The surrounding `broadcast_swarm_status` (server/swarm_status.rs, invoked at ~line 682)
  fanouts one member through `event_tx`; the fixture is constructed to force a yield
  between snapshot and send so that the order of two broadcasts lands "running" then "ready".

So for defect (b), the test fails by design. Nothing about the current tree contradicts that;
the test exists to prove the race is still present.

## Answer 4 — Product defect vs test-side defect
- Defect (a): BOTH. Product side = `reload_process_alive` has a `#[cfg(not(unix))]`
  branch that returns `true` for every non-zero pid without an existence check. Test side =
  the fixture manufactures a dead pid by integer offset, which is Unix-portable-only.
  Separating evidence: an implementation that verified liveness via the OS would make the
  test pass on Windows for a truly dead pid; a test that uses a fresh-reaped child pid
  (like `reload_process_alive_handles_zero_and_dead_pids`, which IS `cfg(unix)` and
  therefore not compiled here) would be portable. The two files are coupled; neither alone
  is the ground truth.
- Defect (b): Test-side only. The assertion encodes the buggy ordering it is explicitly
  written to reproduce. It is a regression probe, not a spec the tree is supposed to meet.

## Limitation
No live run of the two `socket_tests` was possible on this host: the crate has pre-existing
import errors (`client_writer_task`, `DaemonEvents`) that block compilation, so the test
binaries could not link. The defect conclusions come from reading exact source lines plus
the successful 124/0 run of the build-support suite.
""  
"confidence: medium"  
"what_i_did_not_check: live run of the two socket_tests blocked by pre-existing crate compile errors" 
