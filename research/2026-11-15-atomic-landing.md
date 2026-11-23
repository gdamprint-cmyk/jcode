# Atomic landing: BUILD stages, LANDING commits

Date: 2026-11-15
Branch: `fix/transport-access-denied-means-present`
Scope: upstream change to jcode's build/publish/landing mechanics.

## The defect

MEASURED 2026-11-14 on this host: the client launcher ran
`v0.99.684-dev (04265bcce)`, the running daemon ran
`v0.99.681-dev (e8d6ba898)`, and the two channels disagreed for 14 hours.

Root cause, measured in the source:

- `crates/jcode-build-support/src/lib.rs` -
  `publish_local_current_build_for_source()` called `update_current_symlink()`
  and `update_launcher_symlink_to_current()`. A BUILD moved the client
  channels.
- `crates/jcode-app-core/src/tool/selfdev/reload.rs` - only the RELOAD called
  `update_shared_server_symlink()`. A RELOAD moved the daemon channel.

So every build opened a window in which a new client could attach to an older
daemon. The window was real and was measured open for 14 hours.

## The fix

### 1. BUILD is staging-only

`stage_local_current_build_for_source()` (crates/jcode-build-support/src/lib.rs)
does what a build is allowed to do:

- validate the binary against the source state,
- install the immutable versioned binary under `builds/versions/<label>/`,
- smoke-test it,
- record the label in the `staged-version` marker.

It does NOT move `current`, the launcher, or `shared-server`.

Callers switched to the staging path:
- `crates/jcode-app-core/src/tool/selfdev/build_queue.rs` (the queued build),
- `crates/jcode-app-core/src/tool/selfdev/reload.rs` (the self-dev reload),
- `crates/jcode-app-core/src/server/debug_command_exec.rs` (the debug path),
- `src/cli/selfdev.rs` (the self-dev client CLI).

### 2. LANDING is one crash-safe transaction

`crates/jcode-build-support/src/landing.rs` implements the transaction:

| Phase | What moved | What is provable |
|---|---|---|
| `Begun` | nothing | the journal records the previous channel versions |
| `SharedServerPrepared` | `shared-server` only | the client is still on the old build |
| `HandoffRequested` | nothing | the handoff timestamp anchors the identity check |
| `DaemonVerified` | nothing | the daemon PROVES it runs the target |
| `ClientsActivated` | `current` + launcher | the client channels follow the daemon |

The client channels move LAST, and only after the daemon has been proven to
run the target. A failure before that leaves the client on the old build and
restores the daemon channel.

### 3. The proof is the daemon's own identity

The daemon publishes `pid`, `exe`, `git_hash`, `version` and `started_at` to
`builds/daemon-identity.json` at startup
(`crates/jcode-app-core/src/server/landing_identity.rs`).

The landing accepts that identity as proof of the target only when BOTH:

- the commit matches (`version_commit(label)` vs `identity.git_hash`), and
- `identity.started_at >= handoff_requested_at`.

The second condition is what distinguishes "the daemon the handoff produced"
from "the old daemon's identity file that happens to still be on disk". The
version label carries a working-tree fingerprint, so the comparison is by
COMMIT, not by the full label.

Why this is needed: a reload command's exit code is not proof. The daemon's
own reload guard refuses to exec into an older or identical binary, and a
refused reload leaves the daemon running the OLD build while the command still
reports success.

### 4. Crash recovery

An unfinished landing leaves a journal behind
(`builds/landing-transaction.json`). At daemon startup the daemon publishes
its identity FIRST and then runs the recovery, so the recovery sees the
identity of the daemon that is actually up.

- The daemon runs the target -> the transaction completes forward (the client
  channels are moved and the journal is cleared).
- The daemon does not run the target -> the transaction is rolled back (the
  daemon channel is restored; the client channels were never moved).

The recovery takes the single-writer lock first. If a landing is in progress,
the recovery does nothing: that landing owns the channels.

### 5. Single-writer lock

`LandingLock` - `flock(LOCK_EX|LOCK_NB)` on unix, an exclusively created file
carrying the owner's pid on Windows (broken when the owner is gone, so a
killed carrier cannot wedge the landing).

### 6. The CLI

`jcode server land [VERSION] [--json]` (src/cli/args.rs, src/cli/dispatch.rs,
src/cli/commands.rs). It runs the whole transaction and exits non-zero when
the landing was rolled back, so a caller that only checks the exit status
cannot mistake a failed landing for a successful one.

The transaction is driven through `LandingSession`
(crates/jcode-build-support/src/landing.rs), which owns the single-writer lock
and the journal for the whole run. The steps are separate methods because the
HANDOFF between `prepare` and `verify` is the one step that needs an async
runtime: the CLI drives it inside an `async` block, while the rest of the
transaction stays synchronous.

`land_version()` is the synchronous convenience form for callers whose handoff
is not async (the tests).

## Measured results

`cargo test -p jcode-build-support`:

- baseline (HEAD `cec14bfbb`, my changes stashed): **48 passed, 10 failed**
- with the change: **66 passed, 10 failed**

The 10 failures are the SAME 10 in both runs
(`paths::tests::resolve_binary_payload_follows_channel_symlink_to_wrapper`,
`paths::tests::unix_selfdev_build_keeps_using_the_wrapper`,
`tests::normal_shared_server_candidate_repairs_stale_shared_channel_to_stable`,
`tests::selfdev_reload_target_diverges_from_update_probe_when_shared_server_pinned`,
`tests::shared_server_candidate_prefers_approved_channel_over_current`,
`tests::test_client_update_candidate_prefers_dev_binary_for_selfdev`,
`tests::update_advances_daemon_reload_target_when_shared_server_tracks_stable`,
`tests::update_launcher_symlink_stays_inside_sandbox_home`,
`tests::update_leaves_daemon_reload_target_stale_when_shared_server_pinned_to_selfdev`,
`version_gc::tests::keeps_protected_recent_and_fresh_versions`).
They are a PRE-EXISTING defect on this host, not a regression from this
change. They were measured by stashing the change and re-running the same
command.

The 18 new tests cover: a successful landing moves both channel families; a
failed handoff leaves the client on the old version and restores the daemon
channel; a stale identity is rejected; recovery rolls back and completes
forward; the lock excludes a second landing; an invalid version label is
refused; staging moves no channel; the staged build is the landing target.

## Provenance

- `std::fs::rename` replaces the destination atomically
  (https://doc.rust-lang.org/std/fs/fn.rename.html, read 2026-11-15) - the
  basis for writing the journal through a temp file + rename.
- `flock(2)` (https://man7.org/linux/man-pages/man2/flock.2.html, read
  2026-11-15) - `LOCK_EX|LOCK_NB` returns `EWOULDBLOCK` when the lock is
  held, and the lock is released when the holder dies.
- `std::env::current_exe` and `std::process::id`
  (https://doc.rust-lang.org/std/env/fn.current_exe.html,
  https://doc.rust-lang.org/std/process/fn.id.html, read 2026-11-15) - the
  identity fields.

## Not covered by this change

- The landing is proven in unit tests against a temp home, not live on this
  host. The live proof is the Harbor acceptance run after the carrier lands
  the build.
- `jcode server promote` + `jcode server reload` remain available and are
  still NOT atomic. The Harbor carrier uses `jcode server land` when the
  installed binary supports it and falls back to the old pair otherwise.
