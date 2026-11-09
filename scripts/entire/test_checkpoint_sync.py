#!/usr/bin/env python
"""Repository-local acceptance check for Entire checkpoint publication.

Every local checkpoint ref must be published on the dedicated checkpoint
remote at its CURRENT tip, and the CLI's push queue must be drained.

Why this exists: the CLI discovers refs to push ONLY through the queue file
.git/entire-checkpoint-push-queue.jsonl. A ref written by anything other than
the CLI (e.g. a manual `git commit-tree` + `git update-ref` backfill) stays
local-only forever and is silently never uploaded. This check detects that
class of failure without trusting the CLI's own JSON output.

The correct invariant is NOT "local ref count == remote ref count". A
fast-forward update replaces the remote SHA in place, so counts can legitimately
differ. The invariant is: every local ref is published at its current tip.

Usage:
    python scripts/entire/test_checkpoint_sync.py
    python scripts/entire/test_checkpoint_sync.py --repo <path> --remote <url>

Exit code 0 on PASS, 1 on FAIL.
"""
# reference: upstream entire-cli cmd/entire/cli/checkpoint/pushqueue.go:27 - the
#   queue file is the ONLY push-discovery mechanism; a ref that misses it stays
#   local-only forever (upstream comment at refs_store.go:199-210)
# reference: upstream entire-cli cmd/entire/cli/strategy/push_common.go:211-234
#   - batchPushCheckpointRefs pushes each queued ref at its current tip
# reference: scripts/entire/test_backfill_payloads.py - the sibling repository-local
#   acceptance test; same conventions (provenance header, negative control, RESULT line)

import argparse
import json
import os
import subprocess
import sys

DEFAULT_REMOTE = "https://github.com/gdamprint-cmyk/jcode-checkpoints.git"
CHECKPOINT_PREFIX = "refs/entire/checkpoints/"


def git(repo, *args):
    r = subprocess.run(["git"] + list(args), cwd=repo, capture_output=True)
    if r.returncode != 0:
        raise SystemExit("git %s failed: %s" % (" ".join(args),
                                                r.stderr.decode("utf-8", "replace").strip()))
    return r.stdout.decode("utf-8", "replace")


def local_refs(repo):
    out = git(repo, "for-each-ref", "--format=%(refname) %(objectname)",
              CHECKPOINT_PREFIX)
    refs = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            refs[parts[0]] = parts[1]
    return refs


def remote_refs(repo, remote):
    out = git(repo, "ls-remote", remote, CHECKPOINT_PREFIX + "*")
    refs = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            refs[parts[1]] = parts[0]
    return refs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", default=os.path.dirname(os.path.dirname(
        os.path.dirname(os.path.abspath(__file__)))))
    ap.add_argument("--remote", default=DEFAULT_REMOTE)
    args = ap.parse_args()
    repo = args.repo

    local = local_refs(repo)
    remote = remote_refs(repo, args.remote)

    unpublished = {n: h for n, h in local.items() if remote.get(n) != h}
    remote_only = {n: h for n, h in remote.items() if n not in local}

    queue_path = os.path.join(repo, ".git", "entire-checkpoint-push-queue.jsonl")
    queue_entries = []
    if os.path.exists(queue_path):
        with open(queue_path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    queue_entries.append(json.loads(line))
                except ValueError:
                    queue_entries.append({"ref": "<unparseable>", "hash": "<unparseable>",
                                          "raw": line})

    print("=" * 78)
    print("checkpoint sync check")
    print("  repo  :", repo)
    print("  remote:", args.remote)
    print("=" * 78)
    print("local refs        :", len(local))
    print("remote refs       :", len(remote))
    print("unpublished       :", len(unpublished))
    print("remote-only       :", len(remote_only))
    print("queued entries    :", len(queue_entries))
    for name, sha in sorted(unpublished.items()):
        print("  UNPUBLISHED %s local=%s remote=%s" % (name, sha, remote.get(name)))
    for name, sha in sorted(remote_only.items()):
        print("  REMOTE-ONLY %s remote=%s" % (name, sha))
    for e in queue_entries:
        print("  QUEUED %s" % e.get("ref"))

    # NEGATIVE CONTROL: the checker must be able to FAIL. If there is no local
    # checkpoint ref at all, the check must report FAIL rather than vacuously
    # pass. An empty run is exactly how a broken remote would look.
    if not local:
        print()
        print("NEGATIVE CONTROL: no local checkpoint refs found - FAIL")
        print("RESULT: FAIL")
        return 1

    ok = not unpublished and not remote_only and not queue_entries
    print()
    if ok:
        print("every local ref is published at its current tip")
        print("RESULT: PASS")
        return 0
    print("RESULT: FAIL")
    return 1


if __name__ == "__main__":
    sys.exit(main())
