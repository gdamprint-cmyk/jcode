# v10 lane A: handoff-parity validation (READ-ONLY)

Scope: read-only. No repo file edited, no cargo, no landing, no reload, no daemon restart.
Tree read: `C:\Users\WorkShop1\proj\jcode-src`, HEAD `e5455cede` (working tree dirty with other
sessions' staged edits; `crates/jcode-build-support/src/landing.rs` was NOT in `git status`
modified list, so the quoted bytes are the committed ones).
Python executor read: `C:\Users\WorkShop1\.jcode\harbor\harbor_land.py` (634 lines) plus every
`*.py` in that directory for the analogous predicate.

---

## 1. Did 697daa6a9 change exactly the `None` arm and nothing else in that function?

CONFIRMED, for the function body. The commit touched three files: `landing.rs`,
`landing_tests.rs`, `tool/selfdev/reload.rs`. In `landing.rs` the diff has three hunks:
`@@ -571,6 +571,19 @@` (doc-comment block added above the function), `@@ -581,7 +594,9 @@`
(the `None` arm), `@@ -1459,6 +1474,23 @@` (a new test). The ONLY executable change in the
function is `None => true` -> `None => false`. The `Some` arm and the version-match guard are
byte-identical.

Verbatim diff of the arm (`git show 697daa6a9 -- crates/jcode-build-support/src/landing.rs`):

```diff
@@ -581,7 +594,9 @@ pub fn daemon_identity_proves_version(
     }
     match handoff_requested_at {
         Some(requested_at) => identity.started_at >= requested_at,
-        None => true,
+        // No handoff timestamp means no way to tell "the daemon re-exec'd onto
+        // the target" from "the old daemon's identity file is still on disk".
+        None => false,
     }
 }
```

Current source, `crates/jcode-build-support/src/landing.rs:564-601` (verbatim):

```rust
/// Whether the daemon identity proves the daemon runs `version` AND is
/// fresher than the handoff request.
///
/// Both facts are required. The commit proves which build runs; the
/// timestamp proves it is the daemon that the handoff produced. Without the
/// timestamp, an identity left behind by the OLD daemon would make a failed
/// handoff look successful.
///
/// Returns `false` when no identity has been published: an unverifiable
/// daemon must never be treated as a verified one.
///
/// MEASURED 2026-11-17: `None` is NOT a weaker proof, it is NO proof at all.
/// A transaction that crashed before `record_handoff_requested` carries
/// `handoff_requested_at: None` (landing.rs:1026), and this function used to
/// accept the identity on the git-hash match alone. The git hash cannot
/// distinguish two builds of the SAME commit - the source fingerprint that
/// does distinguish them lives only in the version LABEL, never in the binary
/// and never in the identity - so a stale identity published by an older
/// daemon of the same commit was accepted as proof that the target runs.
///
/// The honest verdict for "no handoff timestamp" is "unprovable", not
/// "probably fine": the landing rolls back in that case, which is the safe
/// direction. A rollback is recoverable; a desynced ecosystem is not.
pub fn daemon_identity_proves_version(
    identity: &DaemonIdentity,
    version: &str,
    handoff_requested_at: Option<DateTime<Utc>>,
) -> bool {
    if !daemon_identity_matches_version(identity, version) {
        return false;
    }
    match handoff_requested_at {
        Some(requested_at) => identity.started_at >= requested_at,
        // No handoff timestamp means no way to tell "the daemon re-exec'd onto
        // the target" from "the old daemon's identity file is still on disk".
        None => false,
    }
}
```

Note the context claim in the brief (`landing.rs:1026` for the `None` init) is STALE by line
drift: the field is initialised at `landing.rs:1041` and stamped at `landing.rs:1086-1093`.
The doc comment inside the source still cites `:1026`; that is a stale citation, not a code
defect. Verbatim, `crates/jcode-build-support/src/landing.rs:1082-1093`:

```rust
/// Record that the daemon handoff was requested.
///
/// The timestamp anchors the identity check: any daemon identity published
/// before this moment describes the OLD daemon.
pub fn record_handoff_requested(transaction: &mut LandingTransaction) -> Result<()> {
    transaction.handoff_requested_at = Some(Utc::now());
    advance_landing(
        transaction,
        LandingPhase::HandoffRequested,
        "handoff requested",
    )
}
```

---

## 2. The Python executor: is there an equivalent "no timestamp => accept anyway" hole?

ANSWER: no literal equivalent, and the Python side is actually STRICTER in one respect. It
never reads the daemon-identity file at all, so it cannot have the Rust `None` hole. Its
freshness anchor is the promote marker's mtime, and the predicate degrades to `True` when
either input is missing - a missing-evidence-accepts shape, but on different inputs
(process `create_time` / marker mtime), not on a handoff timestamp.

`harbor_land.py` imports only stdlib (`errno, json, os, subprocess, sys, time, pathlib`), so
there is no imported module that could carry a second copy of the decision. All 634 lines
were read; the whole file's `import`/`from` set is:

```python
import errno
import json
import os
import subprocess
import sys
import time
from pathlib import Path
```

### 2a. The analogous decision: `daemon_runs_channel`

`C:\Users\WorkShop1\.jcode\harbor\harbor_land.py:253-281` (verbatim):

```python
def daemon_runs_channel(channel_key, promote_secs=None, pids=None):
    """True when a live daemon runs the CURRENT shared-server channel file.

    Two facts are required, and both are needed on Windows:

      1. the daemon's exe IS the channel file (same NTFS file index), and
      2. the daemon STARTED at or after the channel was promoted.

    Fact 2 is what makes fact 1 meaningful. The channel is a COPY, so a daemon
    that started before the promote keeps running the OLD image while its exe
    PATH already names the new file - it would pass a path-only check while
    still serving stale code. MEASURED 2026-11-14 13:28Z: pid 46708 is such a
    daemon (it started before the promote and binds the 8.3-spelled socket, a
    separate server), while pid 22956 is the one the landing exec'd.
    """
    if channel_key is None:
        return False
    rows = daemon_processes()
    if pids is not None:
        rows = [row for row in rows if row["pid"] in pids]
    for row in rows:
        if row["file_key"] != channel_key:
            continue
        started = row.get("create_time")
        if (started is not None and promote_secs is not None
                and started < promote_secs):
            continue
        return True
    return False
```

The freshness stamp, `harbor_land.py:233-250` (verbatim):

```python
def promote_anchor():
    """When the shared-server channel was LAST PROMOTED - the promote anchor.

    MEASURED 2026-11-14 13:36Z: the channel FILE's mtime is the BUILD time
    (14:47:15), not the promote time. atomic_symlink_swap COPIES the version
    binary onto the channel path, and the copy keeps the source's mtime. So the
    channel file cannot be the anchor - using it made the landing proof accept
    a daemon that predated the promote (caught by the negative control).

    The `shared-server-version` MARKER file is the anchor: promote_version_to_
    shared_server writes it with std::fs::write at promote time, so its mtime
    is exactly when the channel was re-pointed (MEASURED: 18:26:32, the
    promote this session performed).
    """
    try:
        return (BUILDS / "shared-server-version").stat().st_mtime
    except OSError:
        return None
```

So: the Python proof DOES require a stamp (`promote_secs`), and it is read at
`harbor_land.py:570` and passed at `:583`. It has no "no stamp => accept" branch analogous to
the Rust `None => true`. What it does have is the mirror-image weakness at `:277-279`: the
comparison is skipped (`started`/`promote_secs` being `None` falls through to `return True`).
`promote_secs` is `None` only when the marker file is unreadable, and `started` is `None` only
when psutil cannot supply `create_time`. Both are missing-evidence cases, not
"deliberately absent handoff timestamp" cases. The stronger separate check is
`harbor_land.py:586-587`, which does not depend on any timestamp:

```python
    if not new_pids:
        problems.append("no new daemon process appeared: the reload did not exec")
```

### 2b. Where the Python side actually decides, and the ordering asymmetry

`harbor_land.py:537-553` runs the atomic landing through the TARGET's own binary and trusts
its exit code; the four-fact proof is a POST-hoc check at `:568-591`:

```python
    rc, out = run_cli(["server", "land", target, "--json"],
                      timeout=RELOAD_TIMEOUT_SECS, binary=target_binary)
    if rc == 0:
        say("land rc=0 %s" % out.strip()[:300])
    elif _is_unknown_subcommand(out):
        ...
        return 1
    else:
        say("FAIL land did not succeed")
        return 1
```

Consequence worth recording: the Python executor's acceptance of "the daemon runs the target"
is the RUST verdict inside `jcode server land` (the `daemon_identity_proves_version` path). The
Python post-check would not catch a regression that made the Rust proof accept a stale
identity, because by then the client channels have already been moved and `harbor_land.py`
only reports `problems` - it does not roll back (`:600-603` returns 1 after the fact). The
Python P3 is a second, differently-shaped proof (live process + file identity + start time),
not a re-implementation of the identity proof. So the two holes are not duplicates: the Rust
hole was inside the gate; the Python side has no gate of that shape.

---

## 3. Any OTHER Rust call site passing `None` as `handoff_requested_at`?

NO second production hole. Two independent strategies, both negative beyond the two
legitimate sites, plus one adjacent call site that passes `None` but is a DIFFERENT function.

Strategy 1 - `git grep -n 'daemon_identity_proves_version' -- '*.rs'` over the tracked tree
(HEAD `e5455cede`). Nine matches in three files; the only invocation sites are
`landing.rs:702` and `landing.rs:778`. Full match list:

```
crates/jcode-app-core/src/build.rs:23:    daemon_identity_path, daemon_identity_proves_version, daemon_runs_version,
crates/jcode-build-support/src/landing.rs:587:pub fn daemon_identity_proves_version(
crates/jcode-build-support/src/landing.rs:702:        Ok(Some(identity)) => daemon_identity_proves_version(
crates/jcode-build-support/src/landing.rs:766:/// The proof has TWO parts (see [`daemon_identity_proves_version`]):
crates/jcode-build-support/src/landing.rs:778:            && daemon_identity_proves_version(&identity, target_version, handoff_requested_at)
crates/jcode-build-support/src/landing.rs:1459:        assert!(!daemon_identity_proves_version(
crates/jcode-build-support/src/landing.rs:1470:        assert!(daemon_identity_proves_version(
crates/jcode-build-support/src/landing.rs:1489:            !daemon_identity_proves_version(&identity, "abc1234-dirty-deadbeef", None),
crates/jcode-build-support/src/lib.rs:29:    daemon_identity_proves_version, daemon_runs_version, decide_landing_recovery,
```

Strategy 2 - independent instrument: `rg -n --no-heading 'daemon_identity_proves_version' crates`
(ripgrep, different index/regex engine, working-tree bytes) and
`agentgrep(mode=grep, path=crates, query=daemon_identity_proves_version)`. Both returned the
same nine match lines in the same three files. `agentgrep` reported "matches: 9 in 3 files".

Both invocations pass a value that is `None`-capable, and both are correct by construction -
they forward the transaction's own field, and the `None` case is exactly what the fix now
refuses:

`crates/jcode-build-support/src/landing.rs:699-709` (verbatim):

```rust
/// Whether the live daemon provably runs `version` for this transaction.
pub fn daemon_runs_version(transaction: &LandingTransaction) -> bool {
    match read_daemon_identity() {
        Ok(Some(identity)) => daemon_identity_proves_version(
            &identity,
            &transaction.target_version,
            transaction.handoff_requested_at,
        ),
        _ => false,
    }
}
```

`crates/jcode-build-support/src/landing.rs:770-790` (verbatim):

```rust
pub fn wait_for_daemon_identity(
    target_version: &str,
    handoff_requested_at: Option<DateTime<Utc>>,
    timeout: std::time::Duration,
) -> Result<DaemonIdentity> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(identity)) = read_daemon_identity()
            && daemon_identity_proves_version(&identity, target_version, handoff_requested_at)
        {
            return Ok(identity);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "the daemon did not report running {target_version} within {}s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(DAEMON_IDENTITY_POLL);
    }
}
```

`wait_for_daemon_identity`'s own callers, both of which forward a real stamp or a
`None` that is legitimately refused:

- `landing.rs:890-896` (`LandingSession::verify_and_activate`) forwards
  `self.transaction.handoff_requested_at`, which was stamped by `record_handoff()`
  (`landing.rs:882-884`) immediately before the reload. CLI driver: `src/cli/commands.rs:2459`
  then `:2468`. So a CLI landing always carries a stamp.
- `landing.rs:803-808` (`activate_clients_after_verified_daemon`) forwards its parameter.
  Its ONLY caller is `crates/jcode-app-core/src/tool/selfdev/reload.rs:532-536`, which after
  697daa6a9 passes `Some(reload_handoff_floor)`. Verified with the same two instruments:
  `git grep -n 'activate_clients_after_verified_daemon' -- '*.rs'` and
  `rg -n 'activate_clients_after_verified_daemon' crates`; both return exactly one call site
  (reload.rs:532), the two re-exports (`build.rs:14`, `lib.rs:27`), and the definition
  (`landing.rs:803`). So the fix closed the only `None` argument that existed.

### The one adjacent `None` that is NOT a hole (checked, then ruled out)

`crates/jcode-build-support/src/landing.rs` also has a stamp-free activation path,
`activate_clients_without_daemon` (`landing.rs`, reached from `src/cli/commands.rs:2452-2457`),
which moves the client channels WITHOUT any handoff timestamp and without any identity proof:

```rust
        if !crate::server::has_live_listener(&crate::server::socket_path()).await {
            crate::build::log_landing_step(
                "no daemon is running; activating the client channels directly",
            );
            return Ok(session.activate_clients_without_daemon()?);
        }
```

This is deliberate and is not `daemon_identity_proves_version(..., None)`: it is guarded by a
live-listener probe, and its safety comes from there being no daemon to desync from (the next
daemon starts from the `shared-server` channel `prepare` just pointed at the target). It does
NOT pass `None` to the proof function. Recorded so the "second hole" question is answered
completely: the only stamp-free channel moves are (a) this listener-absent path, (b) the
`None` arm of the proof, now `false`.

### Note on the negative-control reach

`landing_tests.rs` calls `daemon_identity_proves_version` only indirectly (through
`wait_for_daemon_identity` at `landing_tests.rs:391-395`, which passes
`transaction.handoff_requested_at` after `record_handoff_requested` at `:383`). The direct
`None` call at `landing.rs:1489` is the regression test added by the fix.

---

## Bottom line

1. CONFIRMED: the only executable change to the function is `None => true` -> `None => false`;
   the diff hunk `@@ -581,7 +594,9 @@` is the whole behavioural delta.
2. NO equivalent hole in `harbor_land.py`: it has no identity-file read and no handoff
   timestamp; it requires a promote anchor (`:583`) plus a new PID (`:586`). Its weakness is
   the opposite shape - `:277-279` accepts when `create_time`/`promote_secs` is missing.
   Structural asymmetry: the Python post-check cannot detect a regression of the Rust gate,
   because the channels have already moved when it runs and it does not roll back.
3. NO second `None` call site: `git grep` and `rg` agree on one production invocation pair
   (`landing.rs:702`, `landing.rs:778`), and `activate_clients_after_verified_daemon`'s sole
   caller now passes `Some(reload_handoff_floor)`.

confidence: high
what_i_did_not_check: did not run cargo/test and did not execute harbor_land.py, so the verdicts are source-reading only (no runtime confirmation that a None-arm transaction actually rolls back on this host).
