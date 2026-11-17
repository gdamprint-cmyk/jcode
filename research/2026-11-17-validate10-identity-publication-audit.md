# Identity Publication Audit — Harbor BUILD-vs-LANDING proof class
Date: 2026-11-17 | Working dir: C:\Users\WorkShop1\proj\jcode-src | Status: READ-ONLY

## 1. `started_at` is the publication/write time
- Source: `crates/jcode-app-core/src/server/landing_identity.rs:45-54`
  `pub fn publish_current_daemon_identity()` builds `DaemonIdentity { ..., started_at: chrono::Utc::now() }`.
- The doc comment at `landing_identity.rs:36-44` states this explicitly:
  "The `started_at` timestamp is the moment the IDENTITY was published, not the moment the process started."
- Evidence from this host: `%LOCALAPPDATA%\jcode\builds\daemon-identity.json` mtime
  `2026-11-17T05:02:58.4469303Z` equals the content `"started_at":"2026-11-17T05:02:58.446584600Z"`
  (sub-millisecond), while the daemon process (pid 33552) was created at `05:02:44 UTC`
  (~14 s EARLIER). So started_at tracks the WRITE (publish) time, not process creation.
- Source of the clock: `chrono::Utc::now()` evaluated at the moment
  `publish_current_daemon_identity` runs, which is inside the daemon's startup sequence
  at `server.rs:2601` (after listener bind, before accept loops).
- NOTE: only the NEW daemon's publish overwrites the file; the OLD daemon's identity is
  preserved only as `daemon-identity.bak` (see #4).

## 2. The published `pid` is NOT verified against a live process
- Consumer entry point: `daemon_identity_proves_version` at `crates/jcode-build-support/src/landing.rs:574-586`.
  It checks only TWO things: (a) `daemon_identity_matches_version(identity, version)`
  (git hash commit comparison), and (b) timestamp ordering when `handoff_requested_at` is Some.
  It never calls `process_is_alive` (that function exists only at `landing.rs:1341-1362` for
  breaking stale landing locks) and never checks whether `identity.pid` is the current daemon.
- `pid` is informational only: `activate_clients_after_verified_daemon` prints it
  (`landing.rs:796-800`), and `log_landing_step` records it as `daemon pid=...`.
- Why it is safe-ish anyway: the identity file is written by the daemon itself
  (`publish_current_daemon_identity` uses `std::process::id()`), so the pid in the file
  always equals the writer's own pid. There is no cross-check of "is the process the one
  the landing expects to be running the target". A stale file from an already-reaped
  processes passes as long as the hash/timestamp criteria pass.

## 3. Every caller that can publish an identity
A literal-text search (`agentgrep`) found exactly ONE production call site:
- `crates/jcode-app-core/src/server.rs:2601`: `landing_identity::publish_identity_and_recover();`
  inside `Server::run()` — the daemon startup path, reached both by normal startup and by
  any reload/re-exec (the reloaded process is a fresh `run()`).
- The only other references are the module itself
  (`crates/jcode-app-core/src/server/landing_identity.rs:84-91`, exported as `pub(crate)`,
  and `landing_identity.rs:53` calling `publish_daemon_identity`) and test code
  (`crates/jcode-build-support/src/landing_tests.rs` calls `publish_identity` helper at
  `landing_tests.rs:52-63`, and `landing_identity.rs` is `pub(crate)` for tests).
- Module re-exports: `crates/jcode-build-support/src/lib.rs:34` re-exports
  `publish_daemon_identity`, `read_daemon_identity`, `record_handoff_requested`,
  `recover_landing`, `recover_landing_locked`, `rollback_landing`, `save_landing_transaction`,
  `shared_server_is_pinned_to_selfdev`.
- `crates/jcode-app-core/src/build.rs:33` also re-exports `publish_daemon_identity`,
  `publish_local_current_build`, `prune_old_versions` — but nothing in build.rs calls
  `publish_identity_and_recover`.
- CONCLUSION: the publisher set is exactly the daemon startup path (`server.rs:2601`).
  There is no second path (no "re-publish on demand", no "publish on shutdown", no
  "publish when handoff completes") other than that single call site.

## 4. Is the identity file rewritten on every publish? (.bak behaviour)
- `publish_daemon_identity` at `crates/jcode-build-support/src/landing.rs:248-253`
  calls `storage::write_json(&path, identity)`.
- `crates/jcode-storage/src/lib.rs:554-647` `write_json_inner`: writes to a temp file
  `path.with_extension(format!("tmp.{pid}.{nonce}"))`, `fsync`s if durable, then on
  non-Unix (Windows) renames atomically in place:
  `landing.rs:595-624` — if `path.exists()`: remove old `.bak`, then `rename(path, .bak)`
  (Windows: `rename(path, &bak_path)` at `landing.rs:618-620`), then `rename(tmp_path, path)`
  at `landing.rs:626`.
- CONSEQUENCE: EVERY publish overwrites the identity file AND preserves the previous
  content as `daemon-identity.bak` (which itself is replaced on each subsequent publish).
  The disk on this host shows `daemon-identity.json` plus `daemon-identity.bak` from the
  previous daemon — consistent with this. The file is rewritten on EVERY publish; it is not
  created once and never touched.

## 5. What happens to the identity file when the daemon EXITS?
- There is NO deletion of `daemon-identity.json` anywhere in the codebase.
  Searched for `daemon-identity.json` (agentgrep): only 2 hits — `landing.rs:89` (const
  name) and `research/2026-11-15-atomic-landing.md` (documentation reference). No
  `remove_file`/`unlink`/`cleanup` of that path exists anywhere (the `remove_file` search
  across the tree contains no occurrence of `daemon-identity`).
- Therefore a STALE identity file survives daemon exit indefinitely. A later daemon
  overwrites it on its own startup publish (with the prior content saved to `.bak`),
  so the current file always reflects the newest publishing daemon — but on a crash/kill
  before the new daemon publishes, the last legacy file remains and is a candidate for the
  recovery path below. `.bak` files are never pruned.

## 6. Synthesis: crash-recovery with `handoff_requested_at = None` accepts STALE identity
- Crash-recovery path: `recover_landing` (`landing.rs:1180-1188`)
  -> `recover_landing_locked` (`landing.rs:1189-1226`)
  -> `daemon_runs_version` (`landing.rs:685-694`)
  -> `daemon_identity_proves_version` (`landing.rs:574-586`).
- A transaction that CRASHED BEFORE the handoff was recorded has
  `handoff_requested_at: None` (set at `landing.rs:1026` by `begin_landing`).
- `daemon_identity_proves_version` has the arm `None => true` at `landing.rs:582-585`:
  when no handoff timestamp exists, the ONLY remaining criterion is
  `daemon_identity_matches_version`, i.e. the identity's `git_hash` equals the target
  version's hash-shaped part.
- RESULT: a stale identity from an older daemon of the SAME commit is ACCEPTED without
  distinction. Conditions for acceptance:
  1. `daemon-identity.json` exists on disk (published by any previous daemon, including the
     old daemon of the same commit);
  2. `identity.git_hash` matches the target's hash-shaped version label;
  3. `handoff_requested_at` is None (the journal entry was never advanced past `Begun`).
- SELF-DEVS case (rebuild of the same commit as the running daemon): the same loss happens.
  The git hash matches by construction (same commit = same short hash / same dirty-fingerprint
  prefix), so the timestamp is the ONLY thing that distinguishes "the daemon re-exec'd onto
  the new binary" (`publish_current_daemon_identity` runs at startup, writing the new
  timestamp) from "the old daemon still runs the old binary of the same commit" (its file
  still sits on disk, and `publish_identity_and_recover` has NOT re-run yet because the
  process has not started). With `handoff_requested_at = None`, the landing cannot tell the
  two apart: `None => true`. The None case loses the timestamp distinction entirely.
- Contrast with the `Some` case (`landing.rs:583`): `identity.started_at >= requested_at`
  is required, so an identity published before the handoff can never prove the target —
  exactly what `landing_tests.rs:1438-1449` asserts. The None arm is the gap.

## Verdict
The timestamp-protection design is correct for transactions that reached the handoff
(`Some` arm), and the `started_at` field is genuinely the publication time. However,
crash-recovery for a transaction that never recorded a handoff (`handoff_requested_at: None`)
trusts the file hash alone, so a stale identity published by an older daemon of the same
commit is accepted as proof. The proof is also weakened by the absence of any liveness check
on `identity.pid`.
confidence: high
what_i_did_not_check: whether the .bak files beyond the most recent are ever pruned; whether a daemon can call publish_identity_and_recover more than once within one process (re-exec scenario); whether any other module outside server.rs calls landing_identity::publish_identity_and_recover; whether the landing journal's started_at is ever compared with the identity's started_at; whether any Windows-specific path alias could make the identity file resolved from a different tree than the landing reads 
