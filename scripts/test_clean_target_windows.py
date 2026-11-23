# reference: no prior art; contract-test convention copied from
# scripts/test_check_warning_budget.sh in this repo.
"""Contract tests for scripts/clean_target_windows.py.

Runs offline against a synthetic lane tree. Usage:
    python scripts/test_clean_target_windows.py
"""

import importlib.util
import os
import pathlib
import shutil
import sys
import tempfile
import time

SCRIPT = pathlib.Path(__file__).with_name("clean_target_windows.py")
spec = importlib.util.spec_from_file_location("clean_target_windows", SCRIPT)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

failures = []


def check(name, condition):
    if condition:
        print(f"ok: {name}")
    else:
        print(f"FAIL: {name}")
        failures.append(name)


def make_lane(root, name="cargo-target-test"):
    lane = root / name
    (lane / "debug" / "deps").mkdir(parents=True)
    (lane / "debug" / "incremental" / "sess-old").mkdir(parents=True)
    (lane / "debug" / "incremental" / "sess-new").mkdir(parents=True)
    (lane / "linux-compat").mkdir()
    old = lane / "debug" / "deps" / "serde-1111111111111111.rlib"
    new = lane / "debug" / "deps" / "serde-2222222222222222.rlib"
    old.write_bytes(b"x")
    new.write_bytes(b"x")
    stale = time.time() - 40 * 86400
    os.utime(old, (stale, stale))
    os.utime(lane / "debug" / "incremental" / "sess-old", (stale, stale))
    return lane, old, new


def main() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        lane, old, new = make_lane(root)

        # 1) Stale dep generations: the older generation of the same crate goes,
        #    the newest stays.
        stale = mod.stale_dep_generations(lane / "debug" / "deps", time.time() - 7 * 86400)
        check("older dep generation is stale", [p for _s, p in stale] == [old])
        check(
            "nothing stale before the newest is old enough",
            not mod.stale_dep_generations(lane / "debug" / "deps", time.time() - 90 * 86400),
        )

        # 2) Dry-run removes nothing.
        args = mod.main.__wrapped__ if hasattr(mod.main, "__wrapped__") else None
        argv = [
            "--drives",
            str(root),
            "--sweep",
            "7",
            "--activity-window-min",
            "0",
        ]
        import contextlib
        import io

        buf = io.StringIO()
        with contextlib.redirect_stderr(buf):
            mod.main(argv)
        out = buf.getvalue()
        check("dry-run reports the stale generation", "would sweep" in out)
        check("dry-run does not delete", old.exists() and new.exists())

        # 3) --apply deletes only the regenerable content.
        buf = io.StringIO()
        with contextlib.redirect_stderr(buf):
            mod.main(argv + ["--apply"])
        check("apply removed the stale dep generation", not old.exists())
        check("apply kept the newest dep generation", new.exists())
        check("apply removed the compat cache", not (lane / "linux-compat").exists())
        check(
            "apply removed the stale incremental session",
            not (lane / "debug" / "incremental" / "sess-old").exists(),
        )

        # 4) Protected names are never touched.
        protected_root = pathlib.Path(tempfile.mkdtemp())
        try:
            protected = protected_root / "cargo-target-live"
            protected.mkdir()
            (protected / "keep.bin").write_bytes(b"x")
            old_time = time.time() - 90 * 86400
            os.utime(protected / "keep.bin", (old_time, old_time))
            os.utime(protected, (old_time, old_time))
            buf = io.StringIO()
            with contextlib.redirect_stderr(buf):
                mod.main(
                    [
                        "--drives",
                        str(protected_root),
                        "--prune-lanes",
                        "1",
                        "--activity-window-min",
                        "0",
                        "--apply",
                    ]
                )
            check("protected lane survives", (protected / "keep.bin").exists())
        finally:
            shutil.rmtree(protected_root, ignore_errors=True)

    if failures:
        print(f"{len(failures)} clean_target_windows contract case(s) failed", file=sys.stderr)
        return 1
    print("all clean_target_windows contract cases passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
