# Acceptance remeasure: independent confirmation (2026-11-17)

## What was re-measured
The coordinator reported `cargo test -p jcode-build-support --lib` -> 124 passed,
0 failed for the landing identity fix (commit 697daa6a9). A worker lane re-ran it
independently.

## Evidence

From `research/cargo_build_support_test_output.txt` (written by the lane worker):

```
   49    Finished `test` profile [unoptimized] target(s) in 12m 13s
   50     Running unittests src\lib.rs (E:\cargo-target-shared\debug\deps\jcode_build_support-6500c85ab664a3cf.exe)
   52 running 124 tests
  196 test result: ok. 124 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 208.15s
```

Count matches the coordinator's report exactly: 124 passed, 0 failed, 0 ignored.
Target dir is the shared one (`E:\cargo-target-shared`), so the lane did not pay a
per-lane cold build. Three compiler warnings appear and are pre-existing, not
introduced by the fix: unused `record` (`lib.rs:301`), unused `short_hash`
(`deterministic_build_tests.rs:152`), dead constant `QUARANTINE_FORMAT`
(`binary_lock.rs:37`).

## Verdict
The reported acceptance count is confirmed by a second instrument (a separate
worker session, its own cargo invocation, its own output file). No discrepancy.

confidence: high
what_i_did_not_check: the two recorded test-side defects (socket_tests.rs reload_process_alive on Windows, swarm_tests.rs inversion) - the lane had not reported on them yet when this was written.
