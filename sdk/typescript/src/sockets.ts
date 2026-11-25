/**
 * Socket path resolution, mirroring `jcode-harness-api::sockets`.
 *
 * The bridge and every client must resolve the same directory or nothing can
 * connect, so the rules here follow the Rust module exactly.
 *
 * reference: crates/jcode-harness-api/src/sockets.rs (the Rust module this mirrors)
 * reference: crates/jcode-transport/src/windows.rs (the pipe-name derivation this mirrors)
 */

import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { createHash } from "node:crypto";

export function runtimeDir(): string {
  const explicit = process.env.JCODE_RUNTIME_DIR;
  if (explicit) return explicit;
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg) return xdg;
  if (process.platform === "darwin" && process.env.TMPDIR) {
    return process.env.TMPDIR;
  }
  return path.join(os.tmpdir(), `jcode-${userDiscriminator()}`);
}

function userDiscriminator(): string {
  const raw =
    process.platform === "win32"
      ? (process.env.USERNAME ?? process.env.USER)
      : (process.env.UID ?? process.env.USER);
  return sanitize(raw ?? "user");
}

function sanitize(raw: string): string {
  const out = raw
    .split("")
    .filter((ch) => /[A-Za-z0-9\-_]/.test(ch))
    .slice(0, 64)
    .join("");
  return out === "" ? "user" : out;
}

/**
 * The canonical spelling of a socket path's PARENT directory.
 *
 * WHY THIS EXISTS (measured 2026-11-14 on this host)
 *
 * The pipe name is a hash of the path STRING, so two spellings of one
 * directory produce two DIFFERENT pipes and therefore two independent daemons
 * that cannot see each other. Windows hands a Scheduled Task the 8.3 short
 * spelling of the temp directory (`C:\Users\WORKSH~1\...`) while an
 * interactive shell gets the long one (`C:\Users\WorkShop1\...`).
 * MEASURED: the two hash to `d6352b96b284a42d` and `5e984a5ce3694442` - two
 * pipes, two daemons, one shared session database.
 *
 * `realpathSync.native` resolves an 8.3 name to its long form. The plain
 * `realpathSync` does NOT: MEASURED, it returns the short spelling unchanged.
 * The `.native` variant is the one that matches `std::fs::canonicalize` on the
 * Rust side, which is what keeps the two derivations in agreement.
 *
 * Only the parent is resolved. The final component is kept as written: the
 * socket is unlinked and recreated on bind, so the NAME is what matters.
 *
 * Falls back to the raw path when the parent cannot be resolved (it does not
 * exist yet, or the process may not read it). Two unresolvable paths still
 * compare consistently with each other.
 */
export function canonicalParent(parent: string): string {
  try {
    return stripVerbatimPrefix(fs.realpathSync.native(parent));
  } catch {
    return parent;
  }
}

/**
 * Drop the `\\?\` verbatim prefix from a Windows path.
 *
 * The Rust side canonicalizes with `std::fs::canonicalize`, which returns a
 * VERBATIM path (`\\?\C:\...`) on Windows. MEASURED 2026-11-14: hashing that
 * verbatim string gives `2713a047a605d211` instead of `5e984a5ce3694442`,
 * which would break the pipe-name contract pinned by
 * `jcode-transport`'s `pipe_name_matches_the_typescript_sdk`. The Rust side
 * strips it, so this side strips it too.
 *
 * `\\?\C:\dir` becomes `C:\dir`; `\\?\UNC\server\share` becomes
 * `\\server\share`. A path without the prefix is returned unchanged.
 *
 * reference: https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file
 */
export function stripVerbatimPrefix(path: string): string {
  if (path.startsWith("\\\\?\\UNC\\")) return `\\\\${path.slice("\\\\?\\UNC\\".length)}`;
  if (path.startsWith("\\\\?\\")) return path.slice("\\\\?\\".length);
  return path;
}

/**
 * Endpoint the SDK dials for a given socket path.
 *
 * On Unix that is the path itself. On Windows there is no socket file: the
 * bridge publishes a named pipe whose name is derived from the path, so a
 * client that dialled the path would find nothing there. This mirrors
 * `jcode-transport`'s derivation exactly, and the two are pinned together by a
 * test on each side.
 */
export function transportEndpoint(socketPath: string): string {
  if (process.platform !== "win32") return socketPath;

  // Same rule as the Rust side: a readable stem for diagnosis, plus a hash of
  // the normalized path so two different paths never collide.
  const stem =
    (path.parse(socketPath).name.match(/[A-Za-z0-9\-_]/g) ?? []).join("").slice(0, 32) || "jcode";
  const normalized = hashedPath(socketPath).replace(/\\/g, "/").toLowerCase();
  const hash = createHash("sha256").update(normalized).digest("hex").slice(0, 16);
  return `\\\\.\\pipe\\${stem}-${hash}`;
}

/**
 * The path whose string is hashed into the pipe name.
 *
 * The parent directory is resolved, so two spellings of one directory hash to
 * the same pipe. The final component is kept as written.
 */
export function hashedPath(socketPath: string): string {
  const parent = path.dirname(socketPath);
  const fileName = path.basename(socketPath);
  if (!parent || !fileName) return socketPath;
  return path.join(canonicalParent(parent), fileName);
}

/** Path of the versioned harness API socket. `JCODE_API_SOCKET` overrides. */
export function apiSocketPath(): string {
  return process.env.JCODE_API_SOCKET ?? path.join(runtimeDir(), "jcode-api.sock");
}

/** Path of the internal daemon socket. `JCODE_SOCKET` overrides. */
export function legacySocketPath(): string {
  return process.env.JCODE_SOCKET ?? path.join(runtimeDir(), "jcode.sock");
}
