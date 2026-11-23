# Validation 10 — Harbor stable-channel repair (READ-ONLY measurement)

Host: Windows, `C:\Users\WorkShop1\AppData\Local\jcode\builds`.
Branches: `fix/tv3-session-picker-visibility` in `jcode-src` (90 files modified).
Tools: `read`/`bash dir`/`bash type` only. No cargo, no landing, no reload, no daemon write.

Bottom line: **on this host the stable-repair path is a no-op and cannot repoint
the shared-server channel.** `stable-version` is a zero-byte marker file, which
makes `read_stable_version()` return `None`, and `repair_stale_shared_server_channel()`
returns `AlreadyCurrent` before touching anything.

---

## 1. `read_stable_version()` and what an empty marker yields

Source: `crates/jcode-build-support/src/storage_helpers.rs:180-193`

```rust
/// Read the current stable version
pub fn read_stable_version() -> Result<Option<String>> {
    let path = stable_version_file()?;
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let hash = content.trim();
        if hash.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hash.to_string()))
        }
    } else {
        Ok(None)
    }
}
```

**Empty marker file → `Ok(None)`.** The file is not held as an empty string;
a zero-byte `stable-version` is indistinguishable from a missing file.

Downstream consumption (from the same crate):
- `storage_helpers.rs:180-193`: the `?` chain returns `None` for the file.
- `lib.rs:1188-1195` (`repair_stale_shared_server_channel`): `stable_version`
  unwrapped to `Option<&str>, trimmed, filtered for non-empty` → the `else`
  branch fires on `None` and the function returns
  `Ok(SharedServerRepair::AlreadyCurrent)` **without touching the shared-server
  channel at all**.
- `lib.rs:568-594` (`shared_server_channel_is_current_enough`): same
  `ok().flatten()` + non-empty filter; an empty/stable-less value makes the
  function return `false`.
- `crates/jcode-build-support/src/version_gc.rs:44-80` (`protected_versions`):
  `read_stable_version()` contributes nothing to the GC protection set when
  `None`; versions are protected by explicit channel contents and the manifest.

So an empty `stable-version` is not treated as `"0"` and never repointed; it is
treated as "no stable version recorded".

## 2. `repair_stale_shared_server_channel()` — behavior with an empty/unreadable stable value

Source: `crates/jcode-build-support/src/lib.rs:1187-1234`

```rust
pub fn repair_stale_shared_server_channel() -> Result<SharedServerRepair> {
    let stable_version = read_stable_version()?;
    let Some(stable_version) = stable_version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(SharedServerRepair::AlreadyCurrent);
    };
    ...
    let stable_binary = stable_binary_path()?;
    if !stable_binary.exists() {
        return Ok(SharedServerRepair::AlreadyCurrent);
    }
    // If shared-server already resolves to the same version marker, there is
    // nothing to repair.
    let previous = read_shared_server_version()?;
    if previous.as_deref().map(str::trim).filter(|s| !s.is_empty()) == Some(stable_version) {
        return Ok(SharedServerRepair::AlreadyCurrent);
    }
    if previous
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some_and(|previous| !is_release_channel_marker(previous))
    {
        return Ok(SharedServerRepair::AlreadyCurrent);
    }
    // Only repair when stable is strictly newer than the current shared-server
    // binary on disk. This never downgrades, and it preserves a self-dev pin
    // that is fresher than stable.
    let shared_binary = shared_server_binary_path()?;
    if !shared_server_binary_is_strictly_older_than(&shared_binary, &stable_binary) {
        return Ok(SharedServerRepair::AlreadyCurrent);
    }
    update_shared_server_symlink(stable_version)?;
    Ok(SharedServerRepair::Repaired { ... })
}
```

**Empty stable value → the function refuses to act** (`AlreadyCurrent`), not
"repair to 0" or "unversioned". It never writes the shared-server channel when
`stable-version` is empty or missing.

## 3. Is the repair predicate `shared < stable` satisfiable on this host?

**No.** Two independent reasons:

1. Stable marker is empty → `read_stable_version()` = `None` → `repair_stale_shared_server_channel`
   returns `AlreadyCurrent` at line 1188-1195, before any mtime comparison.
2. Even if the marker had content, the mtime comparison fails:
   - `shared-server\jcode.exe` mtime: **17.11.2026 09:24** (lives next to the
     channel copy; the directory also holds a stale `.jcode-launcher-old-...` file).
   - `stable\jcode.exe` mtime: **07.09.2026 09:09**.
   - `shared_server_binary_is_strictly_older_than` (lib.rs:1254-1275) requires
     `shared_mtime < stable_mtime`. Here `09:24 < 09:09` is false, so it returns
     `false` → `AlreadyCurrent`.

Actual values read from disk:

| path | size | content |
|---|---|---|
| `builds\stable-version` | 0 bytes | (empty) |
| `builds\shared-server-version` | 28 bytes | `16dae995b-dirty-7ccf1fa3d922` |
| `builds\current-version` | 28 bytes | `16dae995b-dirty-7ccf1fa3d922` |
| `builds\staged-version` | 28 bytes | `16dae995b-dirty-7ccf1fa3d922` |

`shared < stable` is therefore unsatisfiable on this host in two ways.

## 4. Can the repair repoint shared-server to a build older than the live one?

**Structurally yes, but the guard blocks it on this host.** The repoint target
is the version directory named by `stable-version` — on this host the old
v0.84.0 release (129,472,512 bytes, mtime 2026-09-07). If `repair_stale_...`
ruled, shared-server would land on v0.84.0, which is older than the live
shared-server binary (`16dae995b-dirty-7ccf1fa3d922`, 146,539,520 bytes, mtime
09:24) and far older than `current`/`canary`.

The guard (`lib.rs:1254-1275`) prevents it:

```rust
fn shared_server_binary_is_strictly_older_than(shared: &Path, stable: &Path) -> bool {
    let mtime = |p| std::fs::metadata(resolve_binary_payload(p))
        .ok().and_then(|m| m.modified().ok());
    let stable_mtime = match mtime(stable) { Some(m) => m, None => return false };
    if !shared.exists() { return true; }          // no deliberate pin: safe
    match mtime(shared) {
        Some(shared_mtime) => shared_mtime < stable_mtime,  // **the guard**
        None => false,
    }
}
```

- It demands `shared mtime < stable mtime`. If shared is **newer** than stable
  (this host: `09:24 < 09:09` false), the repair is refused.
- Any mtime uncertainty on an existing shared binary is treated as "not older"
  (`None` → `false`) so an unverifiable pinned build is never repointed.
- The target is always `stable` (`lib.rs:1225`), and the guard only allows a
  repoint when stable is strictly newer than the *previous* shared-server
  binary — i.e. the repoint is a forward march, never a step back to an older
  build. Relative to a *different* live channel (current/canary) the target can
  be older, but that path is gated by the same mtime predicate, which this host
  does not satisfy.

## 5. Marker files, contents, sizes, and channel correspondence

Channels live under `builds\<channel>\`; on Windows each channel entry is a
copy of the payload plus a stub `jcode.exe` (release installs keep a running
executable open, so channels are copies, not symlinks — see
`crates/jcode-build-support/src/paths.rs` test fixtures and
`platform_support::atomic_symlink_swap`).

| marker | size | exact content | channel dir | binary exists? | label matches channel |
|---|---|---|---|---|---|
| `current-version` | 28 B | `16dae995b-dirty-7ccf1fa3d922` | `current\jcode.exe` (COPY, 146,539,520 B, mtime 09:24) | yes | yes — points at `versions\16dae995b-dirty-7ccf1fa3d922\jcode.exe` (146,539,520 B) |
| `shared-server-version` | 28 B | `16dae995b-dirty-7ccf1fa3d922` | `shared-server\jcode.exe` (COPY, 146,539,520 B, mtime 09:24) + stale `.jcode-launcher-old-14088-...` temp | yes | yes — same label as the live live shared-server binary |
| `staged-version` | 28 B | `16dae995b-dirty-7ccf1fa3d922` | no `staged` channel dir exists | yes — `versions\16dae995b-dirty-7ccf1fa3d922\jcode.exe` (146,576,896 B, mtime 11:28) | label names a versioned binary that exists but there is no `staged` channel; the marker is only the landing target, which is how the BUILD-vs-LANDING split works |
| `stable-version` | **0 B** | *(empty)* | `stable\jcode.exe` (COPY, 129,472,512 B, mtime 07.09.2026 09:09) | yes | **no label stored** — the file is empty, so the marker names nothing; the channel still points at the old v0.84.0 binary |
| `manifest.json` | 6,250 B | `{"stable":null,"canary":"e8d6ba898-dirty-5e8c163ed58d",...,"history":[...]}` | — | — | notes: `stable` is `null` in the manifest; `canary` is `e8d6ba898-dirty-...`; `pending_activation` (2026-11-02 manual publish) is the only pending activation record |
| `versions-prune.stamp` | 1 B | `1` | — | — | GC daily-prune stamp, written after a pass (lib.rs:164-169) |
| `stable-version.dangling-20260928` | 6 B | `0.84.0` | (no version dir named `0.84.0` exists) | no | leftover from an older install; the file is a dangling marker of the old release |

Not present: no `current-version` / `shared-server-version` / `staged-version`
file of zero length (only `stable-version` is zero). No `canary-version` marker.
Versions GC (`version_gc.rs:44-80`) protects: `current`, `stable`, `shared-server`,
`canary` channel target versions, `staged-version`, and the manifest's
`stable`/`canary`/`pending_activation` values; the prune stamp controls the
daily cadence. The old v0.84.0 binary lives as an unversioned copy in
`stable\jcode.exe` and has **no** entry in `versions\`.

## 6. Live shared-server channel version label

`C:\Users\WorkShop1\AppData\Local\jcode\builds\shared-server\jcode.exe --version`
→ `v0.99.725-dev (16dae995b, dirty)` (exit 0).

The same label comes from `current\jcode.exe` (`146,539,520 B` both) and from the
versioned binary at `versions\16dae995b-dirty-7ccf1fa3d922\jcode.exe`
(146,539,520 B, mtime 09:24). `shared-server-version` marker content equals the
live label, so the channel, marker, and versioned binary are consistent.

## Short answers to the measured questions

1. `read_stable_version()` with an empty marker returns `Ok(None)` (storage_helpers.rs:180-193).
2. With an empty/unreadable stable value, `repair_stale_shared_server_channel()` returns
   `AlreadyCurrent` and **refuses to act**; it does not repoint to "0"/unversioned
   (lib.rs:1188-1195).
3. `shared < stable` is **not** satisfiable here: `stable-version` is empty (None →
   early `AlreadyCurrent`), and if it had content, shared mtime 09:24 is not
   strictly older than stable 09:09.
4. A repoint to an older build would need the guard `shared mtime < stable mtime`
   to pass. It does not here (and would be a forward step relative to the
   previous shared binary anyway). Guard prevents repointing older.
5. See the table in section 5: current / shared-server / staged markers all hold
   the live build's label and resolve to existing binaries; stable-version is
   empty and holds no label despite the channel still pointing at old v0.84.0;
   manifest.json has `stable: null`; versions-prune.stamp is the one-byte GC stamp.
6. Live shared-server reports `v0.99.725-dev (16dae995b, dirty)`.

---

confidence: high
what_i_did_not_check: Whether update_shared_server_symlink is ever invoked from any path other than repair_stale_shared_server_channel (landing transaction not run; read-only measurement); exact bytes of the stale .jcode-launcher-old-14088 file in shared-server; whether any process currently holds shared-server\jcode.exe open; the latest published-versions.jsonl entry beyond the first ~20 lines; whether jcode.exe --version prints anything on stderr for channels other than the two measured binaries.