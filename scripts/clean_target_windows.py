# reference: no prior art on this host; ported from scripts/clean_target.sh in
# this repo (bash: /proc, du -sb, find -printf, pgrep) and extended to sweep
# every per-lane cargo-target-* dir on D:/E: instead of a single target dir.
"""Windows port of scripts/clean_target.sh for per-lane Cargo target dirs.

The bash original only handles ONE target directory (CARGO_TARGET_DIR or
repo/target) and relies on /proc, du, find -printf and pgrep. This script
instead sweeps EVERY `cargo-target-*` directory found on the given drives
(D:, E:) and works on Windows.

Safety rules (same spirit as clean_target.sh):
  * a directory used by a live cargo/rustc process is never touched
  * a directory written to within the last N minutes is never touched
  * only regenerable content is removed: cross-compile/compat caches,
    stale artifact generations in deps/, stale incremental sessions
  * dry-run by default; --apply performs the deletions
  * protected names (live target, selfdev, src, shared) are never removed

Usage:
  python scripts/clean_target_windows.py                       # dry-run report
  python scripts/clean_target_windows.py --apply               # real deletion
  python scripts/clean_target_windows.py --sweep 7             # also sweep
                                                            # stale dep gens
  python scripts/clean_target_windows.py --prune-lanes 7 --apply
                                                            # delete whole lane
                                                            # dirs older 7d
  python scripts/clean_target_windows.py --self-test           # offline check

Env:
  JCODE_CLEAN_ACTIVE_WINDOW_MIN   activity window in minutes (default 20)
  JCODE_CLEAN_DRIVES              default "D:E:"
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

DEFAULT_DRIVES = os.environ.get("JCODE_CLEAN_DRIVES", "D:E:")
DEFAULT_WINDOW_MIN = int(os.environ.get("JCODE_CLEAN_ACTIVE_WINDOW_MIN", "20"))

# Never remove these, whatever their age: the live selfdev target, the
# protected cargo caches and the shared target.
DEFAULT_PROTECTED = (
    "cargo-target-live",
    "cargo-target-selfdev",
    "cargo-target-src",
    "cargo-target-shared",
)

# Cross-compile / compat caches: not part of the local dev inner loop.
CROSS_GLOBS = ("*-apple-darwin", "*-pc-windows-*", "linux-compat")

PROFILE_DIRS = ("debug", "release", "selfdev", "dev")


def human(num: float) -> str:
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if abs(num) < 1024.0:
            return f"{num:.1f}{unit}"
        num /= 1024.0
    return f"{num:.1f}PB"


def dir_size(path: Path) -> int:
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(root, name))
            except OSError:
                pass
    return total


def newest_mtime(path: Path) -> float:
    """Newest mtime anywhere under `path` (0.0 when nothing readable)."""
    newest = 0.0
    for root, _dirs, files in os.walk(path):
        try:
            newest = max(newest, os.path.getmtime(root))
        except OSError:
            pass
        for name in files:
            try:
                newest = max(newest, os.path.getmtime(os.path.join(root, name)))
            except OSError:
                pass
    return newest


def active_process_paths() -> list[str]:
    """Command lines of live cargo/rustc processes (lowercased)."""
    cmds: list[str] = []
    try:
        import psutil  # type: ignore

        for proc in psutil.process_iter(["name", "cmdline"]):
            try:
                name = (proc.info.get("name") or "").lower()
                if name not in ("cargo.exe", "cargo", "rustc.exe", "rustc"):
                    continue
                cmdline = " ".join(proc.info.get("cmdline") or [])
                if cmdline:
                    cmds.append(cmdline.lower())
            except Exception:
                continue
        return cmds
    except Exception:
        pass
    # Fallback: CIM query through PowerShell.
    try:
        out = subprocess.run(
            [
                "powershell",
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_Process | "
                "Where-Object { $_.Name -in 'cargo.exe','rustc.exe' } | "
                "ForEach-Object { $_.CommandLine }",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        cmds = [line.lower() for line in out.stdout.splitlines() if line.strip()]
    except Exception:
        pass
    return cmds


def is_busy(path: Path, cmds: list[str]) -> bool:
    key = str(path).lower()
    return any(key in cmd for cmd in cmds)


def stale_dep_generations(deps: Path, cutoff: float) -> list[tuple[int, Path]]:
    """Old artifact generations in deps/, keeping the newest per crate.

    Cargo writes `name-<hash>.ext` files and leaves older ones behind after a
    feature or dependency change. Group by (crate, extension), keep the newest
    file of each group, return the older ones older than `cutoff`.
    """
    groups: dict[tuple[str, str], list[tuple[float, int, Path]]] = {}
    try:
        entries = list(deps.iterdir())
    except OSError:
        return []
    for entry in entries:
        if not entry.is_file():
            continue
        name = entry.name
        stem, _, ext = name.rpartition(".")
        if not stem:
            stem, ext = name, ""
        key = stem
        while key and key[-1] in "0123456789abcdef":
            key = key[:-1]
        key = key.rstrip("-") + ext
        try:
            stat = entry.stat()
        except OSError:
            continue
        groups.setdefault((key, ext), []).append((stat.st_mtime, stat.st_size, entry))

    stale: list[tuple[int, Path]] = []
    for items in groups.values():
        items.sort(reverse=True)
        for mtime, size, path in items[1:]:
            if mtime < cutoff:
                stale.append((size, path))
    return stale


def remove_path(path: Path, reason: str, apply: bool) -> int:
    size = dir_size(path)
    if apply:
        try:
            if path.is_symlink() or path.is_file():
                path.unlink()
            else:
                shutil.rmtree(path)
        except OSError as exc:
            log(f"FAILED to remove ({reason}): {path}  [{exc}]")
            return 0
        log(f"removed ({reason}): {path}  [{human(size)}]")
    else:
        log(f"would remove ({reason}): {path}  [{human(size)}]")
    return size


def log(message: str) -> None:
    print(f"clean_target_windows: {message}", file=sys.stderr)


def scan_lane_dirs(drives: list[str]) -> list[Path]:
    found: list[Path] = []
    for drive in drives:
        root = Path(f"{drive}:\\") if len(drive) == 1 else Path(drive)
        if not root.is_dir():
            continue
        try:
            for entry in root.iterdir():
                if entry.is_dir() and entry.name.startswith("cargo-target-"):
                    found.append(entry)
        except OSError as exc:
            log(f"cannot scan {root}: {exc}")
    return sorted(found, key=lambda p: str(p).lower())


def clean_lane_dir(
    lane: Path,
    args: argparse.Namespace,
    cmds: list[str],
    now: float,
) -> int:
    """Return bytes reclaimed (or reclaimable) inside one lane directory."""
    reclaimed = 0
    window = args.activity_window_min * 60
    newest = newest_mtime(lane)
    if newest and now - newest < window:
        log(f"SKIP (written <{args.activity_window_min}min ago): {lane}")
        return 0
    if is_busy(lane, cmds):
        log(f"SKIP (active cargo/rustc process): {lane}")
        return 0

    # 1) Cross-compile / compat caches.
    for pattern in CROSS_GLOBS:
        for cache in lane.glob(pattern):
            if cache.is_dir():
                reclaimed += remove_path(cache, "cross-compile/compat cache", args.apply)

    # 2) Stale artifact generations and stale incremental sessions.
    if args.sweep is not None:
        cutoff = now - args.sweep * 86400
        for profile in PROFILE_DIRS:
            profile_dir = lane / profile
            if not profile_dir.is_dir():
                continue
            if is_busy(profile_dir, cmds):
                log(f"SKIP sweep (active process): {profile_dir}")
                continue
            deps = profile_dir / "deps"
            if deps.is_dir():
                stale = stale_dep_generations(deps, cutoff)
                swept = sum(size for size, _ in stale)
                for size, path in stale:
                    if args.apply:
                        try:
                            path.unlink()
                        except OSError:
                            continue
                    else:
                        log(f"would sweep stale dep generation: {path}  [{human(size)}]")
                if stale:
                    log(
                        f"{'swept' if args.apply else 'would sweep'} {len(stale)} stale dep "
                        f"generations (>={args.sweep}d) from {lane}/{profile}  [{human(swept)}]"
                    )
                reclaimed += swept
            incremental = profile_dir / "incremental"
            if incremental.is_dir():
                for session in incremental.iterdir():
                    if not session.is_dir():
                        continue
                    try:
                        mtime = session.stat().st_mtime
                    except OSError:
                        continue
                    if mtime < cutoff:
                        reclaimed += remove_path(
                            session, f"stale incremental session (>={args.sweep}d)", args.apply
                        )
    return reclaimed


def prune_lane(lane: Path, args: argparse.Namespace, cmds: list[str], now: float) -> int:
    """Delete an entire stale lane directory."""
    window = args.activity_window_min * 60
    newest = newest_mtime(lane)
    if newest and now - newest < window:
        log(f"SKIP lane (written <{args.activity_window_min}min ago): {lane}")
        return 0
    if is_busy(lane, cmds):
        log(f"SKIP lane (active cargo/rustc process): {lane}")
        return 0
    return remove_path(lane, "stale lane target dir", args.apply)


def self_test() -> int:
    """Offline check of the pure helpers against a synthetic tree."""
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        deps = root / "debug" / "deps"
        deps.mkdir(parents=True)
        old = deps / "serde-1111111111111111.rlib"
        new = deps / "serde-2222222222222222.rlib"
        for path in (old, new):
            path.write_bytes(b"x")
        old_time = time.time() - 40 * 86400
        os.utime(old, (old_time, old_time))

        stale = stale_dep_generations(deps, time.time() - 7 * 86400)
        assert [p for _s, p in stale] == [old], stale
        assert not stale_dep_generations(deps, time.time() - 90 * 86400)

        assert human(1536) == "1.5KB", human(1536)
        assert human(1024 ** 3) == "1.0GB"

        lane = root / "cargo-target-x"
        lane.mkdir()
        assert is_busy(lane, [f"cargo build {lane}".lower()])
        assert not is_busy(lane, ["cargo build elsewhere"])
    print("self-test: OK")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Windows Cargo target janitor")
    parser.add_argument("--apply", action="store_true", help="actually delete (default: dry-run)")
    parser.add_argument("--sweep", type=int, default=None, metavar="DAYS",
                        help="sweep stale dep generations / incremental sessions older than DAYS")
    parser.add_argument("--prune-lanes", type=int, default=None, metavar="DAYS",
                        help="delete whole cargo-target-* lane dirs untouched for DAYS")
    parser.add_argument("--activity-window-min", type=int, default=DEFAULT_WINDOW_MIN,
                        help=f"skip dirs written within this many minutes (default {DEFAULT_WINDOW_MIN})")
    parser.add_argument("--drives", default=DEFAULT_DRIVES, help="drive letters to scan (default D:E:)")
    parser.add_argument("--protect", action="append", default=[],
                        help="additional protected directory names (repeatable)")
    parser.add_argument("--self-test", action="store_true", help="run offline helper checks and exit")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    protected = set(DEFAULT_PROTECTED) | set(args.protect)
    drives = [
        d.strip()
        for d in args.drives.replace(";", ",").split(",")
        if d.strip()
    ]
    lanes = scan_lane_dirs(drives)
    cmds = active_process_paths()
    now = time.time()

    log(
        f"drives={drives} lanes={len(lanes)} activity_window={args.activity_window_min}min "
        f"apply={args.apply} sweep={args.sweep} prune_lanes={args.prune_lanes}"
    )

    total = 0
    for lane in lanes:
        if lane.name in protected:
            log(f"SKIP (protected): {lane}")
            continue
        if args.prune_lanes is not None:
            newest = newest_mtime(lane)
            if newest and now - newest < args.prune_lanes * 86400:
                log(f"SKIP lane (touched <{args.prune_lanes}d ago): {lane}")
                continue
            total += prune_lane(lane, args, cmds, now)
            continue
        total += clean_lane_dir(lane, args, cmds, now)

    log(f"total {'reclaimed' if args.apply else 'reclaimable'}: {human(total)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
