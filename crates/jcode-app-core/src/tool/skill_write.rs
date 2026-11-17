// reference: %LOCALAPPDATA%\hermes\hermes-agent\tools\skill_manager_tool.py (Hermes `skill_manage` create/patch/write_file actions and `_validate_frontmatter`)
// reference: %LOCALAPPDATA%\hermes\hermes-agent\tools\skill_manager_guards.py (Hermes read-before-write guard)
//! Skill authoring writes: the agent-facing half of the skill-creation loop.
//!
//! Reference: Hermes Agent's `tools/skill_manager_tool.py` (`skill_manage`
//! action=`create` / `patch` / `write_file` / `remove_file` / `delete`), whose
//! frontmatter validator (`_validate_frontmatter`) and read-before-write guard
//! (`tools/skill_manager_guards.py`) this module ports.
//! Reference: `~/.jcode/decisions/mutation_ledger.py`, the hook ledger this
//! module's mutations also pass through (it needs a configured hook, so it is
//! not guaranteed to be present).
//! Reference: `%LOCALAPPDATA%\hermes\hermes-agent\tools\skill_ledger.py`, the
//! Hermes per-mutation audit ledger whose row shape `curator_ledger` reproduces
//! (read 2026-11-17: `append_entry` writes id/ts/actor/action/skill/evidence/
//! before/after, and every public write path swallows failures).
//! reference: https://docs.rs/sha2/latest/sha2/ - `Sha256::digest(&bytes)`
//! one-shot API (read 2026-11-17).
//!
//! What this adds to jcode: `skill_manage` could only READ skills
//! (`load`, `list`, `reload`, `reload_all`, `read`). An agent could therefore
//! never author a procedure it had just learned, which is the whole point of
//! the learning loop Hermes claims as its differentiator.
//!
//! The guards are deliberately conservative, because a skill is executable
//! instruction the model will follow later without the author present:
//!
//! * a name must be a safe directory name (no traversal, no separators);
//! * frontmatter must parse and must carry `name` + `description`;
//! * the frontmatter `name` must match the directory name, so a skill cannot
//!   be registered under a name it does not declare;
//! * `patch` requires an exact `old_string` and refuses an ambiguous match;
//! * every write is recorded in the mutation ledger by the `pre_tool` hook,
//!   so a bad skill edit is revertible without git.

use anyhow::{Result, bail};
use jcode_core::eol::{normalize_line_endings, resolve_line_ending};
use std::path::{Path, PathBuf};

/// Which skill write the caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillAction {
    /// Write a new `SKILL.md`.
    Create,
    /// Change an existing `SKILL.md`.
    Patch,
    /// Write a support file under an existing skill.
    WriteFile,
}

/// Maximum size of a single skill file. Skills are injected into the system
/// prompt, so an unbounded file is a context-budget bug, not a preference.
pub const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;

/// Maximum length of a skill name.
const MAX_SKILL_NAME_LEN: usize = 64;

/// A skill write request.
#[derive(Debug, Clone, Default)]
pub struct SkillWrite {
    /// Skill directory name, without any path separator.
    pub name: String,
    /// Full SKILL.md body for `create`.
    pub content: Option<String>,
    /// Support-file path relative to the skill directory, for `write_file`.
    pub file_path: Option<String>,
    /// Exact text to replace, for `patch`.
    pub old_string: Option<String>,
    /// Replacement text, for `patch`.
    pub new_string: Option<String>,
}

/// Path of the per-skill mutation ledger.
///
/// `~/.jcode/skills/.curator_ledger.jsonl`, the same path shape Hermes uses for
/// `%LOCALAPPDATA%\hermes\skills\.curator_ledger.jsonl`. Overridable so the
/// ledger can be exercised without touching the real skills tree.
pub fn curator_ledger_path() -> PathBuf {
    if let Some(path) = std::env::var_os("JCODE_SKILL_LEDGER_PATH") {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::var_os("JCODE_SKILL_LEDGER_DIR") {
        return PathBuf::from(dir).join(".curator_ledger.jsonl");
    }
    skills_root(None).join(".curator_ledger.jsonl")
}

/// Whether skill writes are journaled, and where.
///
/// `None` disables the ledger entirely; the path is what the tool passes to the
/// write functions. Opt-in via `JCODE_SKILL_LEDGER=on` (default path) or an
/// explicit `JCODE_SKILL_LEDGER_PATH` / `JCODE_SKILL_LEDGER_DIR`.
///
/// The env vars live here, not in `record_skill_mutation`, so a test can pass its
/// own path and cargo's parallel test threads cannot cross-contaminate ledgers.
pub fn ledger_path() -> Option<PathBuf> {
    if std::env::var_os("JCODE_SKILL_LEDGER_PATH").is_some()
        || std::env::var_os("JCODE_SKILL_LEDGER_DIR").is_some()
    {
        return Some(curator_ledger_path());
    }
    match std::env::var("JCODE_SKILL_LEDGER") {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            if value.is_empty() || matches!(value.as_str(), "0" | "off" | "false" | "no") {
                None
            } else {
                Some(curator_ledger_path())
            }
        }
        Err(_) => None,
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
    };
    hex_lower(&digest)
}

/// Lowercase hex of a digest. std-only: a digest is a byte string, and a hex
/// crate would be a dependency added for one line.
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Append one before/after entry to the skill ledger.
///
/// TELEMETRY, NOT A GATE: every failure is swallowed, exactly like Hermes'
/// `append_entry`, because a ledger that can block a skill write loses work.
/// The row is the Hermes shape: `id`, `ts`, `actor`, `action`, `skill`,
/// `evidence`, `before`, `after`, where before/after are `{path, sha256}` lists
/// and `sha256` is null for a file that did not exist on that side.
pub fn record_skill_mutation(
    ledger: Option<&Path>,
    action: &str,
    skill: &str,
    path: &Path,
    before: Option<&[u8]>,
) {
    // An explicit path, not an env var read here: cargo runs tests in parallel
    // threads and they must not share this state (Rust Book, ch. 11).
    let Some(ledger) = ledger else {
        return;
    };
    let entry = serde_json::json!({
        "id": ledger_id(),
        "ts": chrono::Utc::now().to_rfc3339(),
        "actor": "agent",
        "action": action,
        "skill": skill,
        "evidence": {},
        "before": [ledger_file(path, before)],
        "after": [ledger_file_after(path)],
    });
    let Some(parent) = ledger.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    use std::io::Write as _;
    // Bytes, not `writeln!`: the file is opened with implicit text-mode newline
    // translation on Windows, and a translated terminator changes the row's bytes
    // on every append. JSONL is defined by the LF terminator.
    let mut line = line.into_bytes();
    line.push(b'\n');
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ledger)
    {
        let _ = file.write_all(&line);
    }
}

/// The `before`/`after` file manifest. `None` is a side the caller did not
/// capture bytes for, which for `before` is a file that did not exist; it stays
/// null rather than being filled from disk, so a `create` cannot claim a
/// before-state it never had.
fn ledger_file(path: &Path, bytes: Option<&[u8]>) -> serde_json::Value {
    let sha256 = bytes.map(sha256_bytes);
    serde_json::json!({
        "path": path.display().to_string(),
        "sha256": sha256,
    })
}

/// The `after` side always comes from the file on disk, which is what the caller
/// wrote; a post-write hash must not be derivable from the request alone.
fn ledger_file_after(path: &Path) -> serde_json::Value {
    let sha256 = std::fs::read(path).ok().map(|bytes| sha256_bytes(&bytes));
    serde_json::json!({
        "path": path.display().to_string(),
        "sha256": sha256,
    })
}

/// 12 hex chars, the same id shape Hermes' `append_entry` writes.
fn ledger_id() -> String {
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect()
}

/// Outcome of a skill write.
#[derive(Debug, Clone)]
pub struct SkillWriteOutcome {
    /// Absolute path of the file that was written.
    pub path: PathBuf,
    /// What happened, for the tool result text.
    pub summary: String,
}

/// Where agent-authored skills are written.
///
/// The global skills root (`~/.jcode/skills/`).
///
/// ALWAYS global, never project-local. A skill is a procedure learned from
/// one session and meant to be reused by later ones; a project-local skill
/// lives in a session-scoped overlay, so the procedure would be invisible to
/// the next session that needs it. The `working_dir` argument is accepted so
/// the caller can pass its context, and is deliberately not used for the
/// target path.
pub fn skills_root(_working_dir: Option<&Path>) -> PathBuf {
    crate::storage::jcode_dir()
        .map(|d| d.join("skills"))
        .unwrap_or_else(|_| PathBuf::from(".jcode/skills"))
}

/// Validate a skill name and return it normalized.
///
/// A name becomes a directory name, so it must not contain a separator, must
/// not be a relative-path token, and must stay inside the skills root.
pub fn validate_skill_name(raw: &str) -> Result<String> {
    let name = raw.trim().trim_start_matches('/').trim();
    if name.is_empty() {
        bail!("'name' is required and must not be empty");
    }
    if name.len() > MAX_SKILL_NAME_LEN {
        bail!(
            "skill name is {} chars, the limit is {}",
            name.len(),
            MAX_SKILL_NAME_LEN
        );
    }
    if name.contains('/') || name.contains('\\') {
        bail!("skill name must not contain a path separator: {name:?}");
    }
    if name == "." || name == ".." {
        bail!("skill name must not be a relative-path token: {name:?}");
    }
    if name.starts_with('.') {
        bail!("skill name must not start with a dot: {name:?}");
    }
    // Windows reserved device names would make the directory unopenable.
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        bail!("skill name is a reserved device name: {name:?}");
    }
    Ok(name.to_string())
}

/// Parse and validate a SKILL.md body.
///
/// The same rules the registry applies when it loads a skill, so a file that
/// passes here is a file the registry will accept — a skill written with a
/// broken frontmatter would otherwise be silently invisible.
pub fn validate_skill_document(name: &str, content: &str) -> Result<()> {
    if content.len() > MAX_SKILL_FILE_BYTES {
        bail!(
            "skill file is {} bytes, the limit is {}",
            content.len(),
            MAX_SKILL_FILE_BYTES
        );
    }
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        bail!("SKILL.md must start with YAML frontmatter delimited by '---'");
    }
    let rest = &trimmed[3..];
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md frontmatter is not closed with '---'"))?;
    let yaml = &rest[..end];
    let declared = frontmatter_string(yaml, "name")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md frontmatter must declare 'name'"))?;
    let declared = declared
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string();
    if declared.is_empty() {
        bail!("SKILL.md frontmatter 'name' must not be empty");
    }
    if declared != name {
        bail!("SKILL.md frontmatter name {declared:?} does not match the skill directory {name:?}");
    }
    let description = frontmatter_string(yaml, "description")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md frontmatter must declare 'description'"))?;
    if description.trim().is_empty() {
        bail!("SKILL.md frontmatter 'description' must not be empty");
    }
    Ok(())
}

/// Read a scalar field out of a small YAML frontmatter block.
///
/// Deliberately minimal: frontmatter in this ecosystem is a handful of scalar
/// keys, and a full YAML parse would pull a dependency into the write path
/// for no gain. The registry's own parser is the authority on load; this
/// exists only to reject a file before it is written.
fn frontmatter_string(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((candidate, value)) = line.split_once(':') else {
            continue;
        };
        if candidate.trim() != key {
            continue;
        }
        return Some(value.trim().to_string());
    }
    None
}

/// Apply a patch to a file body, requiring exactly one exact match.
///
/// Ambiguity is refused rather than resolved: two occurrences of the same text
/// mean the author cannot say which one they meant, and guessing would edit
/// the wrong place.
fn apply_patch(content: &str, old_string: &str, new_string: &str) -> Result<String> {
    if old_string.is_empty() {
        bail!("'old_string' is required for a patch and must not be empty");
    }
    let count = content.matches(old_string).count();
    match count {
        0 => bail!(
            "'old_string' was not found in the file; read the file first and copy the exact text"
        ),
        1 => Ok(content.replacen(old_string, new_string, 1)),
        n => bail!(
            "'old_string' occurs {n} times in the file; it must match exactly once, so include more surrounding context"
        ),
    }
}

/// Create a skill's `SKILL.md`.
///
/// Refuses to overwrite an existing skill: overwriting is a patch's job, and
/// silently replacing a learned procedure loses it.
pub fn create_skill(
    root: &Path,
    request: &SkillWrite,
    ledger: Option<&Path>,
) -> Result<SkillWriteOutcome> {
    let name = validate_skill_name(&request.name)?;
    let content = request
        .content
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("'content' is required to create a skill"))?;
    validate_skill_document(&name, content)?;

    let dir = root.join(&name);
    let path = dir.join("SKILL.md");
    if path.exists() {
        bail!(
            "skill {name:?} already exists at {}. Use action=patch to change it, or pick another name.",
            path.display()
        );
    }
    std::fs::create_dir_all(&dir)?;
    let content = normalize_line_endings(content, resolve_line_ending(&path, None));
    let before = std::fs::read(&path).ok();
    std::fs::write(&path, content)?;
    record_skill_mutation(ledger, "create", &name, &path, before.as_deref());
    Ok(SkillWriteOutcome {
        path,
        summary: format!("Created skill {name:?}"),
    })
}

/// Patch an existing skill's `SKILL.md`.
///
/// The patched result must still be a valid skill, so a patch that breaks the
/// frontmatter is refused before it is written rather than after.
pub fn patch_skill(
    root: &Path,
    request: &SkillWrite,
    ledger: Option<&Path>,
) -> Result<SkillWriteOutcome> {
    let name = validate_skill_name(&request.name)?;
    let old_string = request
        .old_string
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("'old_string' is required for a patch"))?;
    let new_string = request.new_string.as_deref().unwrap_or("");

    let path = root.join(&name).join("SKILL.md");
    let current = std::fs::read_to_string(&path).map_err(|error| {
        anyhow::anyhow!(
            "cannot read skill {name:?} at {}: {error}. Use skill_manage action=read first.",
            path.display()
        )
    })?;
    let before = current.as_bytes();
    let updated = apply_patch(&current, old_string, new_string)?;
    validate_skill_document(&name, &updated)?;
    let updated = normalize_line_endings(&updated, resolve_line_ending(&path, Some(&current)));
    std::fs::write(&path, &updated)?;
    record_skill_mutation(ledger, "patch", &name, &path, Some(before));
    Ok(SkillWriteOutcome {
        path,
        summary: format!("Patched skill {name:?}"),
    })
}

/// Write a support file inside a skill's directory.
///
/// Support files are how a skill stays small: the SKILL.md stays a short
/// procedure and the bulk lives in `references/`. The file must stay inside
/// the skill directory — a support file that escapes it is a path traversal.
pub fn write_skill_file(
    root: &Path,
    request: &SkillWrite,
    ledger: Option<&Path>,
) -> Result<SkillWriteOutcome> {
    let name = validate_skill_name(&request.name)?;
    let file_path = request
        .file_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("'file_path' is required to write a skill support file"))?;
    let content = request
        .content
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("'content' is required to write a skill support file"))?;

    let relative = std::path::Path::new(file_path);
    if relative.is_absolute() {
        bail!("'file_path' must be relative to the skill directory");
    }
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => {
                if part == ".." {
                    bail!("'file_path' must not contain '..'");
                }
            }
            std::path::Component::ParentDir => {
                bail!("'file_path' must not contain '..'")
            }
            _ => bail!("'file_path' must be a plain relative path"),
        }
    }
    if content.len() > MAX_SKILL_FILE_BYTES {
        bail!(
            "skill support file is {} bytes, the limit is {}",
            content.len(),
            MAX_SKILL_FILE_BYTES
        );
    }

    let dir = root.join(&name);
    if !dir.join("SKILL.md").exists() {
        bail!(
            "skill {name:?} does not exist under {}. Create it first with action=create.",
            root.display()
        );
    }
    let target = dir.join(relative);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let before = std::fs::read(&target).ok();
    let content = normalize_line_endings(content, resolve_line_ending(&target, None));
    std::fs::write(&target, content)?;
    record_skill_mutation(ledger, "write_file", &name, &target, before.as_deref());
    Ok(SkillWriteOutcome {
        path: target,
        summary: format!("Wrote {file_path} in skill {name:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(name: &str) -> SkillWrite {
        SkillWrite {
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn names_reject_traversal_and_separators() {
        for bad in ["../escape", "a/b", "a\\b", "..", ".hidden", "CON"] {
            assert!(
                validate_skill_name(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn names_are_trimmed_and_slash_stripped() {
        assert_eq!(validate_skill_name(" /my-skill ").unwrap(), "my-skill");
    }

    #[test]
    fn document_validation_requires_matching_frontmatter() {
        let good = "---\nname: demo\ndescription: A demo\n---\n\nBody\n";
        validate_skill_document("demo", good).expect("valid skill");

        let mismatch = "---\nname: other\ndescription: A demo\n---\n\nBody\n";
        assert!(validate_skill_document("demo", mismatch).is_err());

        let no_desc = "---\nname: demo\n---\n\nBody\n";
        assert!(validate_skill_document("demo", no_desc).is_err());

        let no_frontmatter = "Body without frontmatter\n";
        assert!(validate_skill_document("demo", no_frontmatter).is_err());
    }

    #[test]
    fn create_writes_the_file_and_refuses_to_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        let mut request = req("demo");
        request.content = Some("---\nname: demo\ndescription: A demo\n---\n\nBody\n".to_string());

        let outcome = create_skill(&root, &request, None).expect("create");
        assert!(outcome.path.exists());
        assert_eq!(outcome.path.file_name().unwrap(), "SKILL.md");

        let second = create_skill(&root, &request, None);
        assert!(second.is_err(), "an existing skill must not be overwritten");
    }

    #[test]
    fn patch_requires_one_exact_match() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        let mut request = req("demo");
        request.content = Some("---\nname: demo\ndescription: A demo\n---\n\nBody\n".to_string());
        create_skill(&root, &request, None).expect("create");

        let mut patch = req("demo");
        patch.old_string = Some("Body".to_string());
        patch.new_string = Some("New body".to_string());
        let outcome = patch_skill(&root, &patch, None).expect("patch");
        assert!(
            std::fs::read_to_string(outcome.path)
                .unwrap()
                .contains("New body")
        );

        let mut ambiguous = req("demo");
        ambiguous.old_string = Some("e".to_string());
        ambiguous.new_string = Some("E".to_string());
        assert!(
            patch_skill(&root, &ambiguous, None).is_err(),
            "an ambiguous patch must be refused"
        );
    }

    #[test]
    fn patch_refuses_a_result_that_breaks_the_frontmatter() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        let mut request = req("demo");
        request.content = Some("---\nname: demo\ndescription: A demo\n---\n\nBody\n".to_string());
        create_skill(&root, &request, None).expect("create");

        let mut patch = req("demo");
        patch.old_string = Some("description: A demo".to_string());
        patch.new_string = Some("description:".to_string());
        assert!(patch_skill(&root, &patch, None).is_err());
    }

    #[test]
    fn support_files_stay_inside_the_skill_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("skills");
        let mut request = req("demo");
        request.content = Some("---\nname: demo\ndescription: A demo\n---\n\nBody\n".to_string());
        create_skill(&root, &request, None).expect("create");

        let mut support = req("demo");
        support.file_path = Some("references/detail.md".to_string());
        support.content = Some("Reference body\n".to_string());
        let outcome = write_skill_file(&root, &support, None).expect("write support file");
        assert!(outcome.path.ends_with("references/detail.md"));

        let mut escaping = req("demo");
        escaping.file_path = Some("../escape.md".to_string());
        escaping.content = Some("nope\n".to_string());
        assert!(write_skill_file(&root, &escaping, None).is_err());
    }

    /// The ledger is telemetry for a bad skill edit: a write that is not
    /// recorded is an edit nobody can prove or revert.
    #[test]
    fn skill_writes_are_recorded_with_before_and_after_hashes() {
        let temp = tempfile::tempdir().unwrap();
        // The test owns its ledger path. An env var would be process-global and
        // cargo runs tests in parallel threads, so other tests' writes would
        // land in this file.
        let ledger = temp.path().join("ledger.jsonl");
        let root = temp.path().join("skills");

        let mut create = req("ledger-demo");
        create.content = Some("---\nname: ledger-demo\ndescription: A demo\n---\n\nBody\n".to_string());
        create_skill(&root, &create, Some(&ledger)).expect("create");

        let mut patch = req("ledger-demo");
        patch.old_string = Some("Body".to_string());
        patch.new_string = Some("Patched body".to_string());
        patch_skill(&root, &patch, Some(&ledger)).expect("patch");

        // Read the bytes back from disk, never recomputed from a literal: the
        // writer normalizes line endings, so the on-disk bytes are the truth.
        let skill_md = root.join("ledger-demo").join("SKILL.md");
        let after_bytes = std::fs::read(&skill_md).unwrap();
        let expected_after = sha256_bytes(&after_bytes);
        let expected_before = sha256_bytes(
            b"---\nname: ledger-demo\ndescription: A demo\n---\n\nBody\n",
        );

        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&ledger)
            .expect("ledger written")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("valid JSONL"))
            .collect();
        assert_eq!(lines.len(), 2, "one entry per skill write: {lines:?}");

        let create_row = &lines[0];
        assert_eq!(create_row["action"], "create");
        assert_eq!(create_row["skill"], "ledger-demo");
        assert_eq!(
            create_row["before"][0]["sha256"], serde_json::Value::Null,
            "a created file had no before state"
        );
        assert_eq!(create_row["after"][0]["sha256"], expected_before);
        assert_eq!(create_row["after"][0]["path"], skill_md.display().to_string());

        let patch_row = &lines[1];
        assert_eq!(patch_row["action"], "patch");
        assert_eq!(patch_row["before"][0]["sha256"], expected_before);
        assert_eq!(patch_row["after"][0]["sha256"], expected_after);
        assert_ne!(
            patch_row["before"][0]["sha256"], patch_row["after"][0]["sha256"],
            "a real patch must change the hash"
        );

        // NEGATIVE CONTROL: a second patch's before-state must equal the first
        // patch's after-state. If the ledger hashed the post-write file for both
        // sides, this equality would hold but the create row's before would be
        // non-null, so the control distinguishes the two implementations.
        let mut patch2 = req("ledger-demo");
        patch2.old_string = Some("Patched body".to_string());
        patch2.new_string = Some("Third body".to_string());
        patch_skill(&root, &patch2, Some(&ledger)).expect("patch again");
        let lines2: Vec<serde_json::Value> = std::fs::read_to_string(&ledger)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines2.len(), 3);
        assert_eq!(lines2[2]["before"][0]["sha256"], expected_after);
    }
}
