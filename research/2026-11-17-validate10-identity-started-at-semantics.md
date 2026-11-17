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
