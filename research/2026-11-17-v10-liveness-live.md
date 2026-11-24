# Live verification of the Windows reload liveness fix (2026-11-17)

## What was verified
Commit `012ef539d` replaced the Windows arm of `reload_process_alive`
(`crates/jcode-app-core/src/server/reload_state.rs`), which used to be
`let _ = pid; true`, with a real liveness query: `OpenProcess` +
`GetExitCodeProcess` + `STILL_ACTIVE`. Before this, every non-zero pid counted as
alive on Windows, so a stale reload pid could never be reported dead and
`inspect_reload_wait_status` would return `Waiting` forever.

The earlier check was compile-only. This is the BEHAVIOURAL check.

## Probe
`research/cv10-liveness-probe.ps1` mirrors the exact Win32 sequence the fix uses
(`PROCESS_QUERY_LIMITED_INFORMATION` = 0x1000, `STILL_ACTIVE` = 259) via P/Invoke,
then exercises four cases.

## Result

```
self pid=33504 alive=True (expect True)
bogus pid=655000 alive=False (expect False)
pid 0 alive=False (expect False)
negative_control_found_a_dead_pid_in_range=True (expect True)
live_pids_seen=473
```

All four expectations met:

- the probe's own live pid is reported alive;
- a pid outside the used range (655000) is reported dead, where the OLD code
  returned `true` for it unconditionally;
- `pid == 0` short-circuits to false, unchanged;
- NEGATIVE CONTROL: scanning 654000..655000 found a pid the query reports dead,
  so the instrument is not vacuously returning true. Without this the "bogus is
  dead" line would prove nothing.

`live_pids_seen=473` is the host's process count, i.e. the probe ran against a
real, busy process table.

## Scope of the claim
This proves the Win32 liveness predicate behaves correctly on this host. It does
not by itself prove the Rust binary now contains it: the crate does not link on
this tree because six pre-existing unresolved-import errors in other sessions'
uncommitted `client_writer_task` work block `jcode-app-core`. What IS established
for the Rust side is that `reload_state.rs` produces no compiler errors (the only
errors in the build log are the six foreign E0432/E0433), and the same Win32
sequence is what the probe exercised.

confidence: high
what_i_did_not_check: the compiled jcode binary was not run (crate blocked by foreign uncommitted errors), so the Rust call path end to end was not executed; only its exact Win32 core was exercised behaviourally.
