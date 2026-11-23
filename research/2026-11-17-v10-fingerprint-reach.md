# v10: how far does the source fingerprint reach?

READ-ONLY lane. Nothing in `C:\Users\WorkShop1\AppData\Local\jcode\builds` was
written; no repo file edited, no build, no landing.

Scope of the claim: "the fingerprint" means `SourceState::fingerprint`, the 16-hex
FNV-1a value at `crates/jcode-build-support/src/source_state.rs:121`. The shorter
12-hex string in a dirty version label is a PREFIX of it (`&fingerprint[..12]`,
source_state.rs:123).

## 1. Computed where, consumed where, recorded where

**Computed.** One site only:

- `crates/jcode-build-support/src/source_state.rs:110-121` — `current_source_state()`
  folds full hash + porcelain status + binary diff + every untracked file's name,
  length and bytes into FNV-1a, then `let fingerprint = format!("{state:016x}")`.
  The label is derived from it at `:122-126`.

**Consumed.**

| Site | Use |
|---|---|
| `source_state.rs:142-149` `ensure_source_state_matches` | drift guard; refuses to publish when expected != current |
| `deterministic_build.rs:112,187-188,213-217` | the superseded-vs-failed classification |
| `lib.rs:470-483` `publication_fingerprint_for` | picks which fingerprint goes into the ledger row |
| `lib.rs:660-683` `validate_dev_binary_source_metadata` | compares sidecar fields before publishing |
| `landing.rs:152-154,836-864` `LandingTransaction.source_fingerprint` | carried into the journal by `begin_landing` |
| `tool/selfdev/build_queue.rs:658,768-778`, `status.rs:84-87,140-141` | operator display only |

**Recorded (writers, quoted).**

1. `crates/jcode-build-support/src/lib.rs:1063-1067` — the durable append-only ledger:
   ```rust
   record_publication(&PublicationRecord {
       version: source.version_label.clone(),
       source_fingerprint: source.fingerprint.clone(),
   ```
2. `lib.rs:441-446` — the install row of the same ledger, via
   `publication_fingerprint_for(source, version)`; the doc comment at `:460-469`
   states the true value comes from the build sidecar and otherwise falls back to
   the label's own `-dirty-<hex>`.
3. `lib.rs:494-498` `write_dev_binary_source_metadata` →
   `target/selfdev/jcode.exe.source.json` (a SIDECAR NEXT TO THE DEV BINARY,
   never copied into `builds/`).
4. `jcode-app-core/src/tool/selfdev/build_queue.rs:474` —
   `manifest.add_to_history(build::current_build_info(&repo_dir)?)?`, i.e.
   `manifest.json`'s 20-row history (`source_state.rs:215-228` fills
   `BuildInfo::source_fingerprint`).
5. `landing.rs:232-236` `save_landing_transaction` — the journal carries
   `source_fingerprint`, but the journal file is deleted when the landing ends
   (`clear_landing_transaction`, landing.rs:238-245).

The one durable read-back path is therefore (1): `published-versions.jsonl`.

## 2. Live tree measurement

`C:\Users\WorkShop1\AppData\Local\jcode\builds` (top level):

```
canary/  current/  shared-server/  stable/  versions/
current-version              28 B   17.11.2026 18:40
daemon-identity.bak         213 B   17.11.2026 15:08
daemon-identity.json        213 B   17.11.2026 18:40
landing-transaction.bak      92 B   (no live landing journal)
manifest.bak / manifest.json(.bak-*)  6260 B
published-versions.jsonl   5982 B   17.11.2026 16:43
shared-server-version        28 B   17.11.2026 18:40
stable-version                0 B   28.09.2026
staged-version               28 B   17.11.2026 16:43
versions-prune.stamp          1 B   17.11.2026 10:03
```

Live markers: `staged-version`, `current-version`, `shared-server-version` all hold
`bdb5fb2a5-dirty-49d018ab5d74` (28 B each).

`daemon-identity.json` (213 B) verbatim:

```json
{"pid":22512,"exe":"C:\\Users\\WorkShop1\\AppData\\Local\\jcode\\builds\\shared-server\\jcode.exe","git_hash":"bdb5fb2a5","version":"v0.99.731-dev (bdb5fb2a5, dirty)","started_at":"2026-11-17T13:40:17.696916600Z"}
```

Channel binaries: `current\jcode.exe`, `shared-server\jcode.exe` and
`versions\bdb5fb2a5-dirty-49d018ab5d74\jcode.exe` are all 146,634,752 bytes;
`stable\jcode.exe` is 129,472,512.

Hex presence for the live build (`49d018ab5d74767c` full / `49d018ab5d74` short):

| file | full 16 hex | short 12 hex |
|---|---|---|
| `manifest.json` | True | True |
| `published-versions.jsonl` | True | True |
| `staged-version` | False | True |
| `current-version` | False | True |
| `shared-server-version` | False | True |
| `daemon-identity.json` | False | **False** |
| `landing-transaction.json` | MISSING (only `.bak`) | MISSING |

`findstr /s /m /c:"49d018ab5d74"` over the directory returned exactly
`current-version manifest.json published-versions.jsonl shared-server-version
staged-version`.

No `*.source.json` exists anywhere under `builds/` (command: `dir /b /s *.source.json`
→ empty). The sidecar lives only next to the dev binary:
`C:\Users\WorkShop1\proj\jcode-src\target\selfdev\jcode.exe.source.json`, 200 B:

```json
{"version_label":"bdb5fb2a5-dirty-49d018ab5d74","source_fingerprint":"49d018ab5d74767c","short_hash":"bdb5fb2a5","full_hash":"bdb5fb2a511752cb03754d76ecd0f5ec0416a415","dirty":true,"changed_paths":54}
```

Prefix relation, checked mechanically over every ledger row:

```
pwsh: foreach ($line in Get-Content published-versions.jsonl) { $h=$o.version.Split('-dirty-')[1];
       if ($o.source_fingerprint.StartsWith($h)) {$ok++} else {$bad++} }
prefix_match_ok=41 bad=0
```

So the 12-hex label fragment is always the prefix of the 16-hex ledger fingerprint
(41 rows, 0 mismatches, 18 distinct labels).

Historical fingerprints: `46a499260-dirty-8fd897e68386` appears once in
`manifest.json`, twice in `published-versions.jsonl`, zero times in
`daemon-identity.json`, and its `versions\46a499260-dirty-8fd897e68386\` directory is
still on disk. Harbor's own JSON state (`queue.json`, 59 entries, plus every other
`harbor/*.json|.jsonl`) contains the live full fingerprint in **0** files.

## 3. Recoverable from the live binary?

Command (deterministic, native `IndexOf`, latin-1 byte-preserving read):

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File C:\Users\WorkShop1\.jcode\tmp_fp_probe.ps1
# $text = [System.IO.File]::ReadAllText('...\builds\shared-server\jcode.exe',
#                                       [Text.Encoding]::GetEncoding(28591))
# foreach ($s in @('49d018ab5d74767c','bdb5fb2a5-dirty-49d018ab5d74',
#                  'v0.99.731-dev (bdb5fb2a5, dirty)','JCODE_FINGERPRINT'))
#   { "$($text.IndexOf($s))  $s" }
```

Result, reproduced twice with identical output:

```
binary: 146634752 bytes
ABSENT  needle=49d018ab5d74767c
ABSENT  needle=bdb5fb2a5-dirty-49d018ab5d74
FOUND  offset=117927728  needle=v0.99.731-dev (bdb5fb2a5, dirty)
ABSENT  needle=JCODE_FINGERPRINT
```

Cross-check on the historical build (`versions\46a499260-dirty-8fd897e68386\jcode.exe`,
146,965,504 B): regex scan for `v\d+\.\d+\.\d+-dev \(([0-9a-f]{7,9}), dirty\)` found 2
matches, one of them `v0.99.715-dev (46a499260, dirty)`, and neither carried any
fingerprint text.

Cause, from source: the embedded banner is minted by
`crates/jcode-build-meta/build.rs:119-126`:

```rust
let version = if is_release {
    format!("v{} ({})", build_semver, git_hash)
} else if dirty {
    format!("v{}.{}.{}-dev ({}, dirty)", major, minor, patch, git_hash)
} else {
    format!("v{}.{}.{}-dev ({})", major, minor, patch, git_hash)
};
```

`git_hash` comes from `env_or_metadata_or_git(... ["rev-parse","--short","HEAD"])`
(:35-42). There is no fingerprint input anywhere in this script, so
`cargo:rustc-env=JCODE_VERSION` cannot contain one. This is a separate identifier
from the label: the same commit built three times yields three different
fingerprints (`4d92ff088b93fe98`, `4db4cc60cea13be0`, `8fd897e683862594`) yet one
banner string.

## 4. Verdict

**A live daemon identity plus its version string is NOT enough to determine which
source state produced it.** The identity file holds only `git_hash` and the banner
(`DaemonIdentity`, landing.rs:188-199; publisher
`server/landing_identity.rs:45-54`), the banner is keyed on the commit alone
(jcode-build-meta/build.rs:123), and the binary contains neither the fingerprint nor
the label — measured above. Two daemons from two different dirty trees of one commit
are indistinguishable from that pair of artifacts. The code already says so:
`landing.rs:1481-1484` ("The source fingerprint that separates two builds of one
commit lives only in the version label, never in the binary and never in the
identity") and the test at `:1486-1492` asserts that a commit match proves nothing
without a handoff timestamp.

**But the answer is recoverable from the live tree, through the label — which the
lane brief assumed was absent.** The channels and their markers carry
`<short>-dirty-<12hex>` (`staged-version`, `current-version`,
`shared-server-version`, all 28 B). Splitting on `-dirty-` gives the label, and the
ledger maps label → full 16-hex fingerprint: `published-versions.jsonl` rows are
`{"version":"...","source_fingerprint":"...16...","validated":true}`, and all 41
rows satisfy `label_hex == fingerprint[..12]` (measured, 0 mismatches). So:

    current-version -> bdb5fb2a5-dirty-49d018ab5d74
                   -> published-versions.jsonl -> 49d018ab5d74767c

That gives "which source state" for every still-published label, including the
three builds of commit `46a499260`, whose directories also survive. It is a
`O(log n)` lookup that already exists as `published_versions_from_ledger()` /
`validated_versions_from_ledger()` (lib.rs:300-338) — the data is there; what is
missing is a reader that resolves a channel marker to the full fingerprint.

Residual gaps, stated precisely:

- The identity file alone (pid + git_hash + banner + started_at) never identifies a
  source state; only together with a channel marker or the ledger does it resolve.
- Release builds have no `-dirty-<hex>` suffix, so their label carries no fingerprint
  at all; the ledger row is then the only record.
- `manifest.json` truncates to 20 rows, `versions/` is GC'd; the ledger is the only
  non-lossy store, and it records the label it was given — a label that is not in the
  ledger has no second copy.
- The dev-binary sidecar has the full fingerprint but lives outside `builds/` and is
  overwritten by every rebuild.

**Minimal change that makes it recoverable from the daemon itself** (two viable
shapes; the first is smaller):

1. Add `source_fingerprint: Option<String>` to `DaemonIdentity` (landing.rs:188-199)
   and populate it in `publish_current_daemon_identity` (landing.rs:46-52) from
   `<label>.split_once("-dirty-")`. This puts the 12-hex state discriminator in the
   one file every landing already reads, at the cost of one struct field and one
   `split_once`; it changes the identity schema, so a rollout must tolerate a missing
   field (hence `Option`).
2. Feed the fingerprint into the build-metadata script
   (`jcode-build-meta/build.rs:119-126`): add `JCODE_SOURCE_FINGERPRINT` as a sibling
   env output alongside `JCODE_VERSION`, so the banner itself carries
   `(hash, dirty, <16hex>)`. This makes the identity self-contained without a schema
   change, but it invalidates every incremental build (the script is at the bottom of
   the crate graph) and changes the `--version` output users parse.

Either way the durable ledger (`published-versions.jsonl`, lib.rs:1063-1067) is
already sufficient for the read-back; what is missing is only that a reader joins
marker → label → fingerprint. No new storage is required.

confidence: high
what_i_did_not_check: I did not run cargo, land, reload, or restart anything, did not walk C:\Users\WorkShop1 recursively, and did not verify that a release-built binary embeds no fingerprint (release builds were not inspected).