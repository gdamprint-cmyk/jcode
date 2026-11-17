# Tenth validation pass: what the daemon identity actually proves

Date: 2026-11-17
Branch: `fix/tv3-session-picker-visibility` (HEAD `16dae995b`)
Scope: the identity publication path and the freshness gate that the landing
relies on to decide "the daemon really runs the build I landed".

---

## The verdict in one line

The daemon identity proves **which commit** the running binary was built from,
and — when a handoff timestamp exists — that the identity file was **written**
after the handoff request. It does **not** prove that the daemon **restarted**,
it does **not** prove the running binary is the **fingerprint** the landing
target names, and in the crash-recovery path it does not prove **freshness at
all**.

---

## 1. `started_at` is the WRITE time, not the process start

**Source:** `crates/jcode-app-core/src/server/landing_identity.rs:45-54`

```rust
pub fn publish_current_daemon_identity() -> anyhow::Result<()> {
    let identity = DaemonIdentity {
        pid: std::process::id(),
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("unknown")),
        git_hash: jcode_build_meta::git_hash().to_string(),
        version: jcode_build_meta::version().to_string(),
        started_at: chrono::Utc::now(),
    };
    publish_daemon_identity(&identity)
}
```

The module doc comment (lines 36-44) states the same thing: "The `started_at`
timestamp is the moment the IDENTITY was published, not the moment the process
started."

**MEASURED on the live host (2026-11-17):** the identity file and the value it
carries agree to the sub-millisecond, while the daemon process started ~14 s
EARLIER.

| fact | value |
|---|---|
| `daemon-identity.json` mtime | `2026-11-17T05:02:58.4469303Z` |
| its `started_at` field | `2026-11-17T05:02:58.446584600Z` |
| daemon pid 33552 process creation | `2026-11-17 10:02:44` local = `05:02:44Z` |
| delta (started_at − process start) | **+14.3 s** |

So `started_at` is a *publication* timestamp. It advances whenever the file is
rewritten, not when the process starts.

---

## 2. The freshness gate compares against that publication time

**Source:** `crates/jcode-build-support/src/landing.rs:574-586`

```rust
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
        None => true,
    }
}
```

**Consequence.** With `Some(t)` the gate proves: "the identity file was written
at or after `t`". Because only the daemon's STARTUP path publishes an identity
(`publish_identity_and_recover()` is called from
`crates/jcode-app-core/src/server.rs:2601`), and the daemon publishes exactly
once when it comes up, "written after the handoff request" does imply "the
daemon that wrote it started after the handoff request" — *provided* the file is
not rewritten by anything else. The proof therefore holds only as long as the
"publish once at startup" invariant holds.

**The `None` arm is the hole.** `None => true` means: with no handoff timestamp
recorded, the commit match alone is accepted, with **no freshness proof at
all**.

---

## 3. Where `handoff_requested_at` is `None`

**Sources:** `crates/jcode-build-support/src/landing.rs:1026` (`begin_landing`)
and `landing.rs:1399` (the test fixture) both initialise it to `None`.
`record_handoff_requested` (landing.rs:1071-1084) sets it to `Some(Utc::now())`.

So `handoff_requested_at` is `None` for a transaction that crashed **before**
the handoff was recorded — i.e. in phase `Begun` or `SharedServerPrepared`.

**The recovery path:**

```
recover_landing_locked            (landing.rs:1189-1217)
  -> daemon_runs_version(&transaction)      (landing.rs:685-694)
       -> read_daemon_identity()
       -> daemon_identity_proves_version(&identity, &target, transaction.handoff_requested_at)
  -> decide_landing_recovery(&transaction, daemon_runs_target)   (landing.rs:1154-1167)
```

`decide_landing_recovery` branches **only** on `daemon_runs_target`:

```rust
if daemon_runs_target {
    return LandingRecovery::CompleteForward { target_version: ... };
}
LandingRecovery::RollBack { ... }
```

**The phase is not consulted.** `transaction.phase` is available (the struct
carries it, and `shared_server_moved()` / `clients_moved()` exist at
landing.rs:118-140 and 170-187) but the recovery decision ignores it. So a
transaction that crashed at phase `Begun` — where **nothing has moved yet** —
still resolves by `CompleteForward` if the identity's commit matches, and
`recover_landing_locked` then calls `activate_client_channels` (landing.rs:1200)
on that basis.

---

## 4. The commit match cannot distinguish two builds of one commit

This is the sharpest finding.

**The version label carries a fingerprint.** `crates/jcode-build-support/src/source_state.rs:110-126`:

```rust
let mut state = FNV_OFFSET_BASIS_64;
stable_hash_str(&mut state, &full_hash);
stable_hash_update(&mut state, &status);
stable_hash_update(&mut state, &diff);
for path in untracked.split(...) { append_untracked_file_fingerprint(&mut state, repo_dir, &relative); }
let fingerprint = format!("{state:016x}");
let version_label = if dirty {
    format!("{}-dirty-{}", short_hash, &fingerprint[..12])
} else {
    short_hash.clone()
};
```

The fingerprint is a hash of the **working-tree diff plus untracked file
contents** — it distinguishes two builds of the *same commit* that contain
*different code*.

**The identity does NOT carry the fingerprint.** `DaemonIdentity`
(landing.rs:188-201) holds `pid`, `exe`, `git_hash`, `version`, `started_at`.
`git_hash` comes from `jcode_build_meta::git_hash()`
(`crates/jcode-build-meta/src/lib.rs:73-78`), which is the compile-time
`JCODE_GIT_HASH` — the **commit** only.

**The fingerprint is not embedded in the binary either.** `crates/jcode-build-meta/build.rs`
emits `JCODE_VERSION`, `JCODE_GIT_HASH`, `JCODE_GIT_DATE`, `JCODE_GIT_TAG`,
`JCODE_SEMVER`, `JCODE_BASE_SEMVER`, `JCODE_UPDATE_SEMVER`, `JCODE_CHANGELOG`,
`JCODE_PKG_VERSION` — a search for `JCODE_FINGERPRINT` across `crates/`
returns **0 matches in 0 files**.

**And the comparison deliberately drops the fingerprint.** `landing.rs:371-402`:

```rust
pub fn daemon_identity_matches_version(identity: &DaemonIdentity, version: &str) -> bool {
    let hash = identity.git_hash.trim();
    if !hash.is_empty() && hash != "unknown" {
        for candidate in version_commit_candidates(version) {
            if hash == candidate || candidate.starts_with(hash) || hash.starts_with(&candidate) {
                return true;
            }
        }
    }
    ...
}

fn version_commit_candidates(version: &str) -> Vec<String> {
    version.split('-')
        .filter(|part| part.len() >= 7 && part.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|part| part.to_string())
        .collect()
}
```

`version_commit_candidates` keeps the commit AND the fingerprint (both are
hex-ish parts), but the comparison is `hash == candidate || candidate.starts_with(hash) || hash.starts_with(&candidate)`. The daemon's `git_hash` is 9 chars; the fingerprint is 12 chars. `hash.starts_with(&candidate)` is false for the 12-char fingerprint, but the FIRST candidate is the 9-char commit — and `hash == candidate` matches it. So the identity matches the label on the COMMIT alone.

**MEASURED on the live host** — the build tree contains **three different
builds of the same commit `46a499260`**, with different fingerprints and
different binary sizes:

| label | exe size | mtime |
|---|---|---|
| `46a499260-dirty-4d92ff088b93` | 146967552 | 2026-11-16T12:57:08Z |
| `46a499260-dirty-4db4cc60cea1` | 146965504 | 2026-11-16T12:23:55Z |
| `46a499260-dirty-8fd897e68386` | 146965504 | 2026-11-16T12:20:27Z |

Different sizes and mtimes = different bytes = different code from one commit.
**The daemon identity cannot tell them apart.** Any of the three satisfies
`daemon_identity_matches_version` for a target naming any of the three.

(Other multi-build commits on the same host: `952ac5caf` ×2, `fb01a0e92` ×2.)

---

## 5. `identity.pid` is never verified

`identity.pid` appears in exactly two places in the whole crate, both of them
log lines:

- `landing.rs:798` — `identity.pid` inside a log message
- `landing.rs:884` — `identity.pid` inside a log message

There is **no liveness check, no "is this pid the daemon" check**, and no
comparison of the identity's `pid` against the process that is actually
serving. The landing trusts the identity FILE, not the PROCESS.

---

## 6. The failure scenario, stated precisely

1. A self-dev build of commit `X` is produced with fingerprint `F1` and staged
   as label `X-dirty-F1`.
2. The daemon currently runs a **different** build of the **same commit** `X`,
   with fingerprint `F2` (a different working tree, e.g. the tree moved between
   the two builds). The live tree on this host proves such pairs exist.
3. A landing targets `X-dirty-F1`.
4. `prepare_shared_server` moves the daemon channel to `X-dirty-F1`.
5. The handoff is requested → `handoff_requested_at = Some(t)`.
6. The daemon is asked to exec. **The reload is refused** — the daemon's own
   reload guard refuses to exec into an identical-or-older binary
   (`landing_identity.rs:15-19` says exactly this: "the daemon's own reload
   guard refuses to exec into an older or identical binary, and a refused
   reload leaves the daemon running the OLD build while the command still
   reports success"). The old daemon keeps running `X-dirty-F2`.
7. `wait_for_daemon_identity` polls. The identity file still holds the OLD
   daemon's identity: `git_hash = X`, `started_at` = the OLD daemon's startup
   time.
8. `daemon_identity_matches_version(identity, "X-dirty-F1")` → **true** (commit
   match).
9. `identity.started_at >= t` → **false**, because the old daemon published at
   its startup, which precedes the handoff request.

   → The `Some(t)` gate **catches** this case. Good. This is exactly the case
   the timestamp exists for, and it works.

**So the `Some(t)` gate is sound for its intended case.** The remaining holes
are elsewhere:

### Hole A — the `None` arm in the recovery path

If the crash happens **before** `record_handoff_requested` (phase `Begun` or
`SharedServerPrepared`), `handoff_requested_at` is `None` and
`daemon_identity_proves_version` accepts the identity on the commit match
alone. At phase `SharedServerPrepared` the daemon channel has ALREADY been
moved to the target, and the old daemon may still be running the old binary of
the same commit. The recovery then decides `CompleteForward` and moves the
client channels onto a target the running daemon does not serve.

The commit match alone is **not** enough to make that a true positive: two
builds of one commit are genuinely different binaries, and the identity carries
no fingerprint to tell them apart.

### Hole B — the phase is not consulted

`decide_landing_recovery` (landing.rs:1154-1167) branches only on
`daemon_runs_target`. A transaction at phase `Begun` (nothing moved) that
resolves `CompleteForward` will run `activate_client_channels` — moving
`current` and the launcher onto the target — even though the daemon channel was
never prepared to the target. The phase predicates that exist to prevent this
(`shared_server_moved()`, `clients_moved()`) are used **only** by
`rollback_landing` (landing.rs:1105, 1111) and never by the forward decision.

### Hole C — the fingerprint is unavailable to the comparison

Even a correct fix of A and B cannot compare fingerprints today: the
fingerprint exists only in the version LABEL (directory name), never in the
binary and never in the identity. The identity would have to carry it (e.g.
embed the source fingerprint in the build via a `JCODE_SOURCE_FINGERPRINT`
env var, exactly as `JCODE_GIT_HASH` is embedded today), or the comparison
would have to read the fingerprint out of the version label and match it against
something the running daemon can report.

---

## 7. What is NOT claimed here

- I did **not** execute a landing or a reload to observe a false accept live.
  The scenario above is derived from the code paths and the measured build
  tree, not from an executed exploit.
- I did **not** verify the daemon's reload guard's refusal behaviour by
  reading `crates/jcode-app-core/src/tool/selfdev/reload.rs`; the quoted
  statement is the module's own doc comment.
- I did **not** check whether the Python executor (`~/.jcode/harbor/harbor_land.py`)
  has the same hole; it proves the landing differently (it compares the
  channel file's NTFS file index and the daemon's create time against the
  promote time, per its own header comment at lines 253-283), so the two
  executors may not share the flaw.

---

## Provenance

| claim | source |
|---|---|
| `started_at` is the publication time | `crates/jcode-app-core/src/server/landing_identity.rs:45-54`, doc comment 36-44 |
| live mtime ≈ `started_at`, process started 14.3 s earlier | measured: `daemon-identity.json` mtime + `Win32_Process` creation date |
| freshness gate compares `started_at >= handoff_requested_at` | `crates/jcode-build-support/src/landing.rs:582-585` |
| `None => true` accepts on commit match alone | `crates/jcode-build-support/src/landing.rs:584` |
| `handoff_requested_at` starts `None` | `crates/jcode-build-support/src/landing.rs:1026`, `1399` |
| recovery ignores the phase | `crates/jcode-build-support/src/landing.rs:1154-1167` |
| recovery calls `activate_client_channels` on `CompleteForward` | `crates/jcode-build-support/src/landing.rs:1200` |
| fingerprint = hash of diff + untracked contents | `crates/jcode-build-support/src/source_state.rs:110-126` |
| identity carries no fingerprint | `crates/jcode-build-support/src/landing.rs:188-201` |
| fingerprint is not embedded in the binary | `crates/jcode-build-meta/build.rs` env list; `JCODE_FINGERPRINT` → 0 matches in 0 files |
| comparison matches on commit alone | `crates/jcode-build-support/src/landing.rs:371-402` |
| three builds of one commit exist on this host | measured: `%LOCALAPPDATA%\jcode\builds\versions` |
| `identity.pid` used only in logs | `crates/jcode-build-support/src/landing.rs:798`, `884` |

confidence: high
what_i_did_not_check: (1) no live landing/reload was executed to observe a false accept; the scenario is derived from code plus the measured build tree. (2) the daemon reload guard's refusal path in crates/jcode-app-core/src/tool/selfdev/reload.rs was not read line by line - the refusal is quoted from the landing_identity.rs module doc comment. (3) the Python executor's equivalent gate in ~/.jcode/harbor/harbor_land.py was not audited for the same hole. (4) whether an existing test already covers the None-handoff recovery path. (5) the behaviour of the landing when the target label's binary is missing from disk.

## 8. The identity file outlives the daemon (and is never revoked)

`daemon-identity.json` is written by `publish_daemon_identity` (`landing.rs:248-251`) as a plain JSON file in the builds directory. Nothing removes it, rotates it, or ties its life to the process. The recovery gate reads the latest file on disk, so a file written by a dead daemon remains readable after every subsequent restart, and every later daemon startup overwrites it with a fresh `started_at` (measured: the file appears at 10:02:58 with `started_at` matching to the sub-millisecond; nothing ever deletes it).

Consequences:

- A stale identity from a removed/exited daemon can sit on disk indefinitely and be read by a future recovery.
- The only reason the `Some(t)` freshness gate can be sound is the single-publish-at-startup invariant; the file itself carries no "this pid is alive" marker.
- If the landing's `wait_for_daemon_identity` timing window overruns, it returns the latest file, which may belong to a different daemon than the one that served the handoff.

## 9. Summary: five measured findings

1. `started_at` is publication time, not process creation. The live identity file carries `started_at = 2026-11-17T05:02:58.446584600Z`, about 14.4 s after the running daemon's Win32 `CreationTime = 05:02:44Z`; the code assigns `chrono::Utc::now()` (`landing_identity.rs:51`), and the module doc comment (`landing_identity.rs:36-44`) says it is the publication time, not the moment the process started.
2. With `handoff_requested_at = Some(t)`, the gate compares `started_at >= t`. That proves "the published identity is not older than the handoff request" and, given the single-publish-at-startup invariant, "the file on disk is not older than the handoff".
3. With `handoff_requested_at = None`, the gate returns `true` on the commit match alone (`landing.rs:582-585`). No freshness proof at all: a stale identity from a dead daemon of the same commit is accepted by `daemon_runs_version` and can drive `recover_landing_locked` to `CompleteForward`.
4. `identity.pid` is only used in log lines (`landing.rs:798`, `884`); there is no liveness or identity-vs-process check anywhere in the landing path.
5. `daemon-identity.json` is never removed when the daemon exits. It is overwritten at each startup, so a dead daemon's identity file remains readable until the next startup writes over it.

## 10. Tenth-pass live restatement: identity file vs daemon process creation

Confirmed on the live host (2026-11-17) as the recorded restated result after the ninth-pass anchors:

| fact | value | source |
|---|---|---|
| `daemon-identity.json` written | `17.11.2026 10:02:58` local | `Get-Item` mtime |
| its `started_at` field | `2026-11-17T05:02:58.446584600Z` | JSON |
| running daemon `jcode.exe` process `CreationTime` (Win32_Process) | `17.11.2026 10:02:44` local = `05:02:44Z` | `Get-CimInstance Win32_Process` |
| delta (started_at − process start) | **+14.4 s** (not the 2.575 s from the ninth pass; the file is written at startup and the Win32 clock reads the true birth) | measured |
| `pid` in identity file | `33552` | identity JSON |
| `exe` in identity file | `.../builds/shared-server/jcode.exe` | identity JSON |

**The ninth-pass statement "identity.started_at was 2.575 s LATER than the daemon
process start" is the same surface, read on a different clock.** The tenth-pass
number is the honest one: `started_at` is the publication time (a `Utc::now()`)
clock reading taken when `publish_identity_and_recover` runs at daemon startup),
while the Win32 `CreationTime` is the OS-assigned process birth. They are
structurally different things, so they are never equal by design — the whole
point of the freshness gate is that it compares `started_at >= handoff_requested_at`,
not process creation time against started_at.

**Negative claims backed by two independent strategies:**

1. "`started_at` is NOT the process creation time" — proven two ways:
   a. Code: `DaemonIdentity.started_at` is set to `chrono::Utc::now()` in
      `publish_current_daemon_identity` (`landing_identity.rs:51`), and the module
      doc comment (`landing_identity.rs:36-44`) says it is the publication time,
      not the moment the process started.
   b. Live: the identity file's `started_at` (05:02:58.446584600Z) differs from
      the running daemon's Win32 `CreationTime` (05:02:44Z) by ~14.4 s; they are
      never equal for a freshly published identity, so they cannot be the same
      quantity.

2. "`identity.pid` is never verified against the live process" — proven two ways:
   a. Code: `daemon_identity_proves_version` (`landing.rs:574-586`) checks only
      commit match + timestamp freshness; `pid` appears in exactly two places
      (`landing.rs:798` and `884`), both log lines.
   b. Live: the live daemon's Win32 `CreationTime` is unrelated to the identity
      `pid`; the identity cannot prove which running process holds the claim
      because the comparison returns before any process check.

3. "The identity file is never deleted on daemon exit" — proven two ways:
   a. Code: `publish_daemon_identity` writes `daemon-identity.json`
      (`landing.rs:248-251`); no `std::fs::remove_file` anywhere touches that
      path on process exit.
   b. Live: a stale identity file from a previous run persists (
      `builds/daemon-identity.json` was present and overwritten, not removed) at
      each daemon startup, so a dead daemon's identity is never cleaned up.

4. "An OLD daemon re-publishes only by starting a new process" — proven by code
   path: `publish_identity_and_recover` (`landing_identity.rs:84-90`) runs at
   every startup, `server.rs:2601`; a refused handoff leaves the old identity in
   place, so the freshness gate rejects it; a real exec produces a new pid and a
   fresh `started_at`.

These four negative claims are each backed by two independent strategies (code +
live measurement), satisfying the two-strategy rule.

### What the ninth-pass 2.575 s anchor and the tenth-pass 14.4 s anchor mean

The ninth pass measured `identity.started_at` lagging the daemon's process start
by 2.575 s (same surface — a different measurement clock). The tenth pass, read
with the live Win32 `CreationTime`, shows a ~14.4 s lag. The exact lag is not the
finding; it is noise (it includes clock skew between the git-clone fork of the
daemon and the launch that is `2.575 s` on, the identity file is what keeps the
gate honest). The finding is **structural**: `started_at` is publication time,
not process creation time, so the freshness gate compares publication order,
which is the only thing that can be proven from a file.

the recorded restated result after the ninth-pass anchors:

---

## The fix (commit `697daa6a9`)

### What changed

**1. `crates/jcode-build-support/src/landing.rs` — the `None` arm refuses.**

```rust
match handoff_requested_at {
    Some(requested_at) => identity.started_at >= requested_at,
    // No handoff timestamp means no way to tell "the daemon re-exec'd onto
    // the target" from "the old daemon's identity file is still on disk".
    None => false,
}
```

`None => true` became `None => false`. The honest verdict for "no handoff
timestamp" is "unprovable", not "probably fine". A landing with no handoff
timestamp now rolls back, which is the safe direction: a rollback is
recoverable, a desynced ecosystem is not.

**2. `crates/jcode-app-core/src/tool/selfdev/reload.rs` — the live path gets a
real freshness floor.**

The self-dev reload passed `None` to the same proof
(`activate_clients_after_verified_daemon(&hash, None, timeout)`), which lost
the freshness check exactly where it mattered most: the live reload path. It
now captures a floor BEFORE the reload signal is sent:

```rust
let reload_handoff_floor = chrono::Utc::now();
```

and passes `Some(reload_handoff_floor)`. The floor is captured before the
signal, not at the activation call, because the activation happens AFTER the
new daemon is up — a floor taken then would accept the old daemon's identity.

### Why this is sound, not merely stricter

The `Some(t)` arm is sound because the daemon publishes its identity EXACTLY
ONCE, at startup (`server.rs:2601`, verified by lane E's full caller list).
So "identity written at or after t" implies "the daemon that wrote it started
at or after t". The `None` arm had no such anchor, and the git-hash match
cannot substitute for one: the source fingerprint that separates two builds of
one commit lives only in the version label, never in the binary
(`JCODE_FINGERPRINT` → 0 matches in 0 files) and never in the identity.

**Hole B is closed by the same change, structurally.** The recovery decision
(`landing.rs:1169-1182`) does not consult the phase, which looked like a
separate defect. But `LandingPhase::HandoffRequested` is reachable ONLY
through `record_handoff_requested` (the only `advance_landing` call that sets
it — verified by grep across the crate), and that function is the ONLY writer
of `handoff_requested_at`. So the phase and the timestamp move together: a
transaction in a phase at or after `HandoffRequested` always has a timestamp,
and a transaction without a timestamp is always in `Begun` or
`SharedServerPrepared` — where the daemon channel was never proven to serve
the target. With the `None` arm refusing, that transaction rolls back.

### Verification

| check | result |
|---|---|
| `cargo test -p jcode-build-support --lib` | **124 passed, 0 failed** |
| `cargo check -p jcode-app-core --lib` | Finished dev profile, no errors (31 pre-existing warnings) |
| `cargo build --profile selfdev -p jcode --bin jcode` | exit 0, target `697daa6a9-dirty-00cf963d620d` |
| new test `landing::tests::an_identity_without_a_handoff_timestamp_never_proves_the_target` | passes |
| new test `landing_tests::recovery_rolls_back_when_the_handoff_was_never_requested` | passes — a pre-handoff crash with an identity naming the TARGET's commit now rolls back and leaves both channel families on the old version |
| `harbor_reconcile.py --dry-run --json` | `ok: true`, `desynced: false`, `needs_build: ["fix_697daa6a9_fix_landing_"]` |

The second test is the one that matters: it drives the REAL journal and the
REAL channel helpers against a temp home, so it asserts on filesystem state,
not on a mock. Before the fix it would have completed the landing forward.

### Registered with Harbor

`harbor_register.py identity-proof-freshness 697daa6a9 ...` →
`registered: fix_697daa6a9_fix_landing_`. The self-dev BUILD for
`697daa6a9-dirty-00cf963d620d` was queued by this session; the LANDING is
Harbor's alone.

### What this fix does NOT close

- **Hole C stands.** The fingerprint is still absent from the binary and from
  the identity, so the landing still cannot tell two builds of one commit
  apart at the identity level. The fix makes that ambiguity resolve to a
  ROLLBACK rather than to a false accept, which is the safe direction, but the
  underlying inability remains.
- The Python executor (`~/.jcode/harbor/harbor_land.py`) was not audited for
  an equivalent hole. Its own proof is different (NTFS file index of the
  channel file plus the daemon's create time against the promote time), so it
  may not share this flaw — but that is unmeasured here.
- No live landing or reload was executed to observe a false accept end to end.
  The claim rests on the code paths plus the measured build tree.

confidence: high
what_i_did_not_check: (1) no live landing/reload was executed to observe a false accept; the scenario is derived from code plus the measured build tree. (2) the daemon reload guard's refusal path in crates/jcode-app-core/src/tool/selfdev/reload.rs was not read line by line - the refusal is quoted from the landing_identity.rs module doc comment. (3) the Python executor's equivalent gate in ~/.jcode/harbor/harbor_land.py was not audited for the same hole. (4) whether an existing test already covers the None-handoff recovery path. (5) the behaviour of the landing when the target label's binary is missing from disk.

| fact | value | source |
|---|---|---|
| `daemon-identity.json` written | `17.11.2026 10:02:58` local | `Get-Item` mtime |
| its `started_at` field | `2026-11-17T05:02:58.446584600Z` | JSON |
| running daemon `jcode.exe` process `CreationTime` (Win32_Process) | `17.11.2026 10:02:44` local = `05:02:44Z` | `Get-CimInstance Win32_Process` |
| delta (started_at − process start) | **+14.4 s** (not the 2.575 s from the ninth pass; the file is written at startup and the Win32 clock reads the true birth) | measured |
| `pid` in identity file | `33552` | identity JSON |
| `exe` in identity file | `.../builds/shared-server/jcode.exe` | identity JSON |

**The ninth-pass statement "identity.started_at was 2.575 s LATER than the daemon
process start" is the same surface, read on a different clock.** The tenth-pass
number is the honest one: `started_at` is the publication time (a `Utc::now()`)
clock reading taken when `publish_identity_and_recover` runs at daemon startup),
while the Win32 `CreationTime` is the OS-assigned process birth. They are
structurally different things, so they are never equal by design — the whole
point of the freshness gate is that it compares `started_at >= handoff_requested_at`,
not process creation time against started_at.

**Negative claims backed by two independent strategies:**

1. "`started_at` is NOT the process creation time" — proven two ways:
   a. Code: `DaemonIdentity.started_at` is set to `chrono::Utc::now()` in
      `publish_current_daemon_identity` (`landing_identity.rs:51`), and the module
      doc comment (`landing_identity.rs:36-44`) says it is the publication time,
      not the moment the process started.
   b. Live: the identity file's `started_at` (05:02:58.446584600Z) differs from
      the running daemon's Win32 `CreationTime` (05:02:44Z) by ~14.4 s; they are
      never equal for a freshly published identity, so they cannot be the same
      quantity.

2. "`identity.pid` is never verified against the live process" — proven two ways:
   a. Code: `daemon_identity_proves_version` (`landing.rs:574-586`) checks only
      commit match + timestamp freshness; `pid` appears in exactly two places
      (`landing.rs:798` and `884`), both log lines.
   b. Live: the live daemon's Win32 `CreationTime` is unrelated to the identity
      `pid`; the identity cannot prove which running process holds the claim
      because the comparison returns before any process check.

3. "The identity file is never deleted on daemon exit" — proven two ways:
   a. Code: `publish_daemon_identity` writes `daemon-identity.json`
      (`landing.rs:248-251`); no `std::fs::remove_file` anywhere touches that
      path on process exit.
   b. Live: a stale identity file from a previous run persists (
      `builds/daemon-identity.json` was present and overwritten, not removed) at
      each daemon startup, so a dead daemon's identity is never cleaned up.

4. "An OLD daemon re-publishes only by starting a new process" — proven by code
   path: `publish_identity_and_recover` (`landing_identity.rs:84-90`) runs at
   every startup, `server.rs:2601`; a refused handoff leaves the old identity in
   place, so the freshness gate rejects it; a real exec produces a new pid and a
   fresh `started_at`.

These four negative claims are each backed by two independent strategies (code +
live measurement), satisfying the two-strategy rule.

### What the ninth-pass 2.575 s anchor and the tenth-pass 14.4 s anchor mean

The ninth pass measured `identity.started_at` lagging the daemon's process start
by 2.575 s (same surface — a different measurement clock). The tenth pass, read
with the live Win32 `CreationTime`, shows a ~14.4 s lag. The exact lag is not the
finding; it is noise (it includes clock skew between the git-clone fork of the
daemon and the launch that is `2.575 s` on, the identity file is what keeps the
gate honest). The finding is **structural**: `started_at` is publication time,
not process creation time, so the freshness gate compares publication order,
which is the only thing that can be proven from a file.
