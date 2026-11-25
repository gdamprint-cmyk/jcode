# v6 downgrade guard: blind spot — part 2

Date: 2026-11-17
Branch: `fix/6-downgrade-guard-blindspot` (ahead of HEAD `60e7aa06a`)
Scope: the v6 validation pass for the downgrade-guard blind spot — the
**unverifiable build time** and the **missing `stable` marker** in the guard.

---

## The blind spot (in one line)

`would_downgrade()` returns `Ok(false)` — "not a downgrade" — whenever it
cannot read a build time, and `guard_landing_target_is_not_a_downgrade()`
never compares the candidate against the `stable` channel. Together these
two gaps mean a staged build that is *not verified to be newer* than a
*live channel* can be landed without the guard ever refusing it.

---

## 1. `would_downgrade` hides an unreadable build time as "not a downgrade"

**Source:** `crates/jcode-build-support/src/lib.rs:304-314`

```rust
pub fn would_downgrade(candidate: &str, live: &str) -> Result<bool> {
    if candidate == live {
        return Ok(false);
    }
    let (Some(candidate_time), Some(live_time)) =
        (version_build_time(candidate)?, version_build_time(live)?)
    else {
        return Ok(false);
    };
    Ok(candidate_time < live_time)
}
```

`version_build_time()` (lib.rs:285-294) returns `Ok(None)` when
`version_binary_path(version)` does not exist, or when its payload's mtime
cannot be read. The `else` branch then returns `Ok(false)` — deliberately,
per the existing comment ("An UNVERIFIABLE build ... is never treated as a
downgrade: the guard must not block a landing it cannot judge").

The consequence: **an unreadable build time is indistinguishable from a
fresh build**. A candidate whose binary is missing, truncated mid-publish,
or whose mtime is stale/irretrievable passes the guard as if it were
strictly newer than the live channel. The guard never reports "I could not
judge", it reports "not a downgrade".

## 2. The guard's `checked` set omits `stable`

**Source:** `crates/jcode-build-support/src/lib.rs:329-348`

```rust
pub fn guard_landing_target_is_not_a_downgrade(candidate: &str) -> Result<()> {
    let mut checked: Vec<String> = Vec::new();
    for live in [crate::read_shared_server_version()?, crate::read_current_version()?] {
        ...
        checked.push(live.clone());
        if would_downgrade(candidate, &live)? {
            anyhow::bail!(...);
        }
    }
    Ok(())
}
```

The guard iterates **only** `shared-server` and `current`; it never reads
`stable`. The three channels are all live (protected by
`version_gc.rs:46-80`, which keeps `stable`/`current`/`shared-server`/staged
in the prune set), so a downgrade onto `stable` is exactly as damaging as
one onto `current`. `stable` being a later version than the candidate is
never judged.

## 3. How the two gaps compose into a bypass

1. A build is staged at an older commit while the current channels still
   point at the same older commit (stage creates no channel moves; see
   `lib.rs:1005-1010`).
2. `shared-server` and `current` are updated forward to the NEW build by a
   landing's `prepare_shared_server` (`landing.rs:1055-1065`).
3. The staged artifact is now older than the live channels, but
   `would_downgrade(staged, shared_server)` is still `Ok(false)`: the
   staged binary's mtime cannot be read (or the binary was written by a
   partial/crashed publish), so the unreadable build time is reported as
   "not a downgrade".
4. The guard checks only `shared-server` (newer) and `current` (newer) —
   both pass — and **never `stable`**, which sits even farther ahead.

The landing is allowed, the daemon channel moves to the staged older build,
and the ecosystem is split with no refusal.

## 4. The measured verdict

Using `would_downgrade` with a forced-missing (unreadable) binary on the
candidate side:

| case | live channel | candidate | `would_downgrade` | guard result |
|---|---|---|---|---|
| candidate older, build time readable | shared-server = newer | older | `Ok(true)` | **refused** |
| candidate older, build time **unreadable** | shared-server = newer | older | `Ok(false)` | **allowed** |
| candidate matches live | shared-server = current | same | `Ok(false)` | allowed (correct) |

The second row is the blind spot: the guard does not distinguish "candidate
is genuinely newer" from "candidate cannot be judged".

---

## Fix and verification

### Fix
- **`would_downgrade`**: return an explicit `Err` when either build time is
  unreadable, so a landing is *refused* rather than mis-classified. Keep the
  documented "never treat unverifiable as a downgrade" rule, but make the
  refusal surface a clear error instead of a silent `Ok(false)`.
- **`guard_landing_target_is_not_a_downgrade`**: include the `stable`
  marker in the `checked` loop so a staged build older than `stable` is
  refused regardless of `shared-server`/`current`.

### Verification
- `cargo test -p jcode-build-support --lib` — existing unit tests must
  remain green (the two new behaviours are exercised inside the guard and
  the comparison; no existing test asserts the "unreadable => allowed"
  path).
- Live-yardstick: `harbor_reconcile.py --dry-run --json` must report
  `needs_build: []` and `needs_apply: []` for the fix commit, and any
  landing path that previously passed an unverifiable staged build must now
  exit non-zero.

---

## Provenance

- `lib.rs:285-294` — `version_build_time()` (the unreadable-mtime source)
- `lib.rs:304-314` — `would_downgrade()` (the false negative)
- `lib.rs:329-348` — `guard_landing_target_is_not_a_downgrade()` (the
  missing `stable` marker)
- `lib.rs:906-947` — `update_shared_server_symlink` /
  `update_current_symlink` / `update_stable_symlink` (the three live
  markers)
- `landing.rs:1055-1065` — `prepare_shared_server` (how live channels move)
- `version_gc.rs:46-80` — `protected_versions` (all three channels are
  live/prune-protected)
- `paths.rs:568-594` — `shared_server_channel_is_current_enough` /
  `shared_server_channel_newer_than_stable` (the sibling marker comparisons
  that also inform gate decisions)
