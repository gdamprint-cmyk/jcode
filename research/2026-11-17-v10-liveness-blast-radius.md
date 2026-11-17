# Blast radius of the reload liveness fix (2026-11-17)

## Why this check
Commit `012ef539d` changed `reload_process_alive` on Windows from
`let _ = pid; true` to a real liveness query. That changes what
`inspect_reload_wait_status` can return, so every caller had to be re-read: a
behaviour change that fixes one path can break another.

## Callers (instrument: `agentgrep` over `crates`, query `inspect_reload_wait_status`, 30 matches in 4 files)

### 1. `await_reload_handoff` - `crates/jcode-app-core/src/server/reload_state.rs:290-325`
Loop calls `inspect_reload_wait_status`, and on `Waiting { pid }` stores the pid
and awaits a lifecycle event (`:304-312`); anything else returns (`:313-321`).

- BEFORE: on Windows a dead pid still yielded `Waiting`, so this loop waited for
  a lifecycle event that could never come for that pid.
- AFTER: a dead pid falls through to `Idle` (`:281-287`) or `Failed`, so the loop
  TERMINATES instead of hanging. This is strictly better: termination is the
  recoverable outcome. No new failure mode.

### 2. Reconnect - `crates/jcode-tui/src/tui/app/remote/reconnect.rs:593-666`
Four-way match. `Ready` -> retry; `Failed` -> `recover_reloading_server`
(`:607-620`); `Idle` -> same recovery (`:621-636`); `Waiting` -> await the event
(`:637-665`).

- The recovery path is shared by `Failed` and `Idle`, and `recover_reloading_server`
  is the designed response to "no ready replacement server". So the fix moves a
  stuck case into a branch that already knows what to do.
- Loop-safety: the reconnect loop's backoff (`:671-680`) is only skipped when
  a branch returns `Retry`. `Idle`/`Failed` call recovery and return `Retry` only
  if recovery succeeded; otherwise control reaches the backoff sleep. No hot loop
  is introduced.

### 3. `wait_for_reload_handoff_before_reconnect` - same file, `:265-372` (`:297`)
Same match shape as (2); same conclusion.

### 4. `server.rs:656` - import only, inside a test-support helper. No decision.

## The one branch that genuinely changed meaning
`reload_state.rs:230-240`, marker phase `Starting`:

```rust
ReloadPhase::Starting => {
    if reload_process_alive(state.pid) {
        ReloadWaitStatus::Waiting { pid: Some(state.pid) }
    } else {
        ReloadWaitStatus::Failed(Some(format!(
            "reload process {} exited before becoming ready", state.pid)))
    }
}
```

This is exactly what the old Windows arm could never reach: it always took the
`Waiting` branch, so "the reload process exited before becoming ready" was
UNREACHABLE on Windows. Now it is reachable and reports `Failed` with the pid.
That is the intended improvement, and the consumers handle `Failed` by design.

The no-marker branch (`:265-287`) is unchanged in structure; only its predicate
got honest. A dead last-known pid now yields `Idle` instead of `Waiting`.

## Verdict
No harmful consequence found. Every path that changes from `Waiting` to
`Failed`/`Idle` lands in a branch whose purpose is recovery, and the reconnect
backoff prevents a hot loop. The fix removes an unreachable-failure condition
rather than adding one.

confidence: medium
what_i_did_not_check: source reading only - the crate does not link on this tree (six foreign uncommitted `client_writer_task` import errors), so no caller was executed, and no test in `reload_state.rs` was run.
