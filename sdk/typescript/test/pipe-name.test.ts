import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import fs from "node:fs";
import os from "node:os";
import { execSync } from "node:child_process";
import { createHash } from "node:crypto";
import { transportEndpoint, stripVerbatimPrefix } from "../src/sockets.ts";

/**
 * The SDK derives the Windows pipe name independently of `jcode-transport`,
 * because one is Rust and one is TypeScript. A drift between them is invisible
 * on Unix and total on Windows: the client dials a pipe nobody is listening on,
 * so nothing connects at all.
 *
 * `jcode-transport`'s `pipe_name_matches_the_typescript_sdk` asserts the same
 * literal strings from the other side.
 *
 * reference: crates/jcode-transport/src/windows.rs (the Rust module this mirrors)
 */

/** The derivation under test, with the platform's path parser injected. */
function derivePipeName(
  socketPath: string,
  parser: path.PlatformPath,
  canonicalParent: (parent: string) => string = (parent) => parent,
): string {
  const stem =
    (parser.parse(socketPath).name.match(/[A-Za-z0-9\-_]/g) ?? []).join("").slice(0, 32) || "jcode";
  const parent = parser.dirname(socketPath);
  const fileName = parser.basename(socketPath);
  const hashed = parent && fileName ? path.join(canonicalParent(parent), fileName) : socketPath;
  const normalized = hashed.replace(/\\/g, "/").toLowerCase();
  const hash = createHash("sha256").update(normalized).digest("hex").slice(0, 16);
  return `\\\\.\\pipe\\${stem}-${hash}`;
}

test("the Windows pipe name matches jcode-transport exactly", () => {
  // Parsed with win32 semantics regardless of the host, so the check runs in
  // CI on Linux rather than only on a Windows runner.
  for (const [socketPath, expected] of [
    [
      "C:\\Users\\jeremy\\AppData\\Local\\jcode\\run\\jcode-api.sock",
      "\\\\.\\pipe\\jcode-api-5e00c01702e8cfe4",
    ],
    ["C:\\a\\b\\jcode.sock", "\\\\.\\pipe\\jcode-52dfdb00b2f35a71"],
  ] as const) {
    assert.equal(
      derivePipeName(socketPath, path.win32),
      expected,
      `pipe name for ${socketPath} drifted from jcode-transport`,
    );
  }
});

test("case and separators normalize the same way", () => {
  assert.equal(
    derivePipeName("C:\\Temp\\Jcode\\server.sock", path.win32),
    derivePipeName("c:/temp/jcode/server.sock", path.win32),
    "the pipe name must not depend on case or separator style",
  );
});

test("a path with no usable characters still yields a name", () => {
  const derived = derivePipeName("/tmp/!!!.sock", path.posix);
  assert.match(derived, /^\\\\\.\\pipe\\jcode-[0-9a-f]{16}$/);
});

test("the short (8.3) spelling of a directory names the same pipe", { skip: process.platform !== "win32" }, () => {
  // MEASURED 2026-11-14 on this host: a Scheduled Task received the 8.3
  // spelling of the temp directory while an interactive shell received the
  // long one, and the two hashed to different pipes - two daemons, one shared
  // session database.
  //
  // The 8.3 spelling is obtained from the OS at run time, so the test does not
  // depend on this machine's user name.
  const longDir = fs.realpathSync.native(
    fs.mkdtempSync(path.join(os.tmpdir(), "jcode-short-spelling-")),
  );

  const shortDir = shortPathSpelling(longDir);
  assert.ok(shortDir, "the OS must produce an 8.3 spelling for this test to mean anything");
  assert.notEqual(shortDir, longDir, "the 8.3 spelling must differ from the long one");

  // The SHIPPED code must name ONE pipe for both spellings. This is the
  // assertion that matters: `transportEndpoint` is what the client dials.
  assert.equal(
    transportEndpoint(path.win32.join(shortDir, "jcode.sock")),
    transportEndpoint(path.win32.join(longDir, "jcode.sock")),
    "two spellings of one directory must name ONE pipe, or two daemons start",
  );

  // The negative control: WITHOUT the parent resolution they differ, so the
  // test above is not vacuous.
  assert.notEqual(
    derivePipeName(path.win32.join(shortDir, "jcode.sock"), path.win32),
    derivePipeName(path.win32.join(longDir, "jcode.sock"), path.win32),
    "without the resolution the spellings must differ",
  );
});

test("the verbatim prefix is stripped before hashing", () => {
  // MEASURED 2026-11-14: Rust's `std::fs::canonicalize` returns
  // `\\?\C:\Users\WorkShop1\...` on Windows. Hashing that verbatim string
  // gives `2713a047a605d211` instead of `5e984a5ce3694442`, which would
  // change every pipe name on the machine and break the pinned SDK contract.
  assert.equal(stripVerbatimPrefix("\\\\?\\C:\\Users\\WorkShop1\\AppData"), "C:\\Users\\WorkShop1\\AppData");
  assert.equal(stripVerbatimPrefix("\\\\?\\UNC\\server\\share"), "\\\\server\\share");
  assert.equal(
    stripVerbatimPrefix("C:\\plain\\path"),
    "C:\\plain\\path",
    "a path without the prefix must be left alone",
  );
});

/**
 * The 8.3 spelling of `path`, as the OS reports it.
 *
 * `GetShortPathNameW` is the only way to ask for it; Node does not wrap it.
 * Returns undefined when the OS declines (the volume has 8.3 name generation
 * disabled).
 */
function shortPathSpelling(path: string): string | undefined {
  try {
    // `cmd`'s `for %I in (...) do @echo %~sI` expands to the short spelling.
    //
    // `execSync` with `shell: "cmd.exe"`, NOT `execFileSync`: Node's
    // `execFileSync` escapes a `"` into `\"`, and `cmd` reads that backslash
    // literally, so the path came out as `C:\"C:\Users\...\"`. MEASURED
    // 2026-11-14: the escaped form does not exist, and the test failed on
    // ENOENT rather than on the pipe name.
    const out = execSync(`for %I in ("${path}") do @echo %~sI`, {
      encoding: "utf8",
      shell: "cmd.exe",
    });
    // `cmd` echoes the quoted form, and a directory keeps the trailing
    // separator inside the quotes.
    let trimmed = out.trim();
    if (trimmed.startsWith('"') && trimmed.endsWith('"')) {
      trimmed = trimmed.slice(1, -1);
    }
    trimmed = trimmed.replace(/[\\/]+$/, "");
    return trimmed && trimmed !== path ? trimmed : undefined;
  } catch {
    return undefined;
  }
}
