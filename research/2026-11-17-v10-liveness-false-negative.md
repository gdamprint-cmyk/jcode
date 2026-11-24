# False-negative risk in the Windows reload liveness predicate (2026-11-17)

## The risk
Commit `012ef539d` made `reload_process_alive` on Windows ask the OS instead of
returning `true`. The new code treats a failed `OpenProcess` as "dead":

```rust
let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
if handle.is_null() { return false; }
```

If `OpenProcess` fails for a process that is actually ALIVE (access restriction),
the caller `inspect_reload_wait_status` returns `Failed`, and the reconnect path
runs `recover_reloading_server` - it may start a SECOND daemon. That would be a
regression introduced by this fix, so it had to be measured, not assumed.

## Authoritative reference
Microsoft Learn, `OpenProcess` (fetched 2026-11-17):
https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocess

- System Idle Process (0x0): fails with `ERROR_INVALID_PARAMETER` (87).
- System / CSRSS: fails with `ERROR_ACCESS_DENIED` (5) - "their access restrictions
  prevent user-level code from opening them".
- Success: an open handle; failure: NULL, call `GetLastError`.

So a denial exists, but only for specific protected system processes - not for an
ordinary daemon.

## Measurement on this host (elevated=True, 519 processes scanned)

Every running process was run through the exact predicate:

```
processes_total=519
judged_alive=513
judged_dead_but_running=6   (all lasterr=87)
```

The six, identified individually (`research/cv10-denied-identity.txt`):

| pid | name | verdict |
|---|---|---|
| 0 | Idle | EXPECTED dead - MS documents error 87 for the System Idle Process |
| 17736 | conhost | transient - opening with ALL access masks also failed; already exiting |
| 7376, 19632, 22280, 27608, 35300 | git | transient, same signature, short-lived hook processes |
| 23900 | python | transient, same signature |

Each of these failed with 87 under EVERY access mask (0x1000, 0x1400, 0x1FFFFF,
0x0001), which is the signature of a process that is already gone, not of a
protected process (those report 5, not 87).

## The decisive check: the real daemon
All 11 live `jcode` processes were probed, including the daemon that the identity
file names:

```
pid=22512 alive=True   (identity_pid=22512 alive=True)
... all 11 jcode processes alive=True
elevated=True
```

No jcode process was judged dead. The daemon this fix could wrongly declare dead
is in fact reported alive.

## Verdict
The false-negative risk is real in principle (a denied handle reads as dead) but
does not fire for the daemon on this host: protected system processes report
`ERROR_ACCESS_DENIED` (5), while the observed failures were 87 on processes that
were exiting. The failure mode is confined to protected OS processes and
transiently-exiting pids, neither of which is a reload target.

Residual, stated honestly: a reload target that is protected AND alive would be
misread as dead. Not observed here; the reload pid comes from jcode's own marker,
so it is a jcode process, which opens fine.

confidence: medium
what_i_did_not_check: did not construct a protected-process fixture (cannot elevate a test process to System/CSRSS level on this host); the "denied means alive" case rests on the MS documentation plus the observed 87-vs-5 split, not on a live protected-process sample.
