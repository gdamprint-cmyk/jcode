use super::*;

struct ResolvedSearchScope {
    root: Option<String>,
    glob: Option<String>,
}

fn resolved_search_scope(
    ctx: &ToolContext,
    path: Option<&str>,
    file: Option<&str>,
    glob: Option<&str>,
) -> ResolvedSearchScope {
    // `file` scopes grep/find to one exact file when `path` is absent.
    let path = path.or(file);
    let Some(path) = path else {
        return ResolvedSearchScope {
            root: None,
            glob: normalized_agentgrep_glob_owned(glob),
        };
    };

    let resolved = resolve_path_arg(ctx, path);
    if resolved.is_file() {
        let root = resolved
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .display()
            .to_string();
        let glob = resolved
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        return ResolvedSearchScope {
            root: Some(root),
            glob,
        };
    }

    ResolvedSearchScope {
        root: Some(resolved.display().to_string()),
        glob: normalized_agentgrep_glob_owned(glob),
    }
}

pub(super) fn build_grep_args(params: &AgentGrepInput, ctx: &ToolContext) -> Result<GrepArgs> {
    let query = params
        .query
        .clone()
        .ok_or_else(|| anyhow::anyhow!("agentgrep grep requires 'query'"))?;
    let scope = resolved_search_scope(
        ctx,
        params.path.as_deref(),
        params.file.as_deref(),
        params.glob.as_deref(),
    );
    Ok(GrepArgs {
        query,
        regex: params.regex.unwrap_or(false),
        file_type: params.file_type.clone(),
        json: false,
        paths_only: params.paths_only.unwrap_or(false),
        hidden: params.hidden.unwrap_or(false),
        no_ignore: params.no_ignore.unwrap_or(false),
        // Keep following symlinks (pre-v0.1.7 behavior); the guardrail is opt-in.
        no_follow: false,
        path: scope.root,
        glob: scope.glob,
    })
}

pub(super) fn build_find_args(params: &AgentGrepInput, ctx: &ToolContext) -> Result<FindArgs> {
    let query = params.query.as_deref().unwrap_or_default();
    if query.trim().is_empty()
        && params.path.as_deref().is_none_or(str::is_empty)
        && params.file.as_deref().is_none_or(str::is_empty)
        && normalized_agentgrep_glob(params.glob.as_deref()).is_none()
        && params.file_type.as_deref().is_none_or(str::is_empty)
    {
        return Err(anyhow::anyhow!(
            "agentgrep find requires 'query' unless path, glob, or type narrows the search"
        ));
    }
    let scope = resolved_search_scope(
        ctx,
        params.path.as_deref(),
        params.file.as_deref(),
        params.glob.as_deref(),
    );
    Ok(FindArgs {
        query_parts: query.split_whitespace().map(ToOwned::to_owned).collect(),
        file_type: params.file_type.clone(),
        json: false,
        paths_only: params.paths_only.unwrap_or(false),
        debug_score: params.debug_score.unwrap_or(false),
        max_files: params.max_files.unwrap_or(10),
        hidden: params.hidden.unwrap_or(false),
        no_ignore: params.no_ignore.unwrap_or(false),
        // Keep following symlinks (pre-v0.1.7 behavior); the guardrail is opt-in.
        no_follow: false,
        path: scope.root,
        glob: scope.glob,
    })
}

pub(super) fn build_outline_args(
    params: &AgentGrepInput,
    ctx: &ToolContext,
    context_json_path: Option<&Path>,
) -> Result<OutlineArgs> {
    // Agents sometimes point `path` at the file itself, either instead of or
    // in addition to `file`. Treat a file-valued `path` as the outline target
    // so the file argument is not joined onto it (for example,
    // ".../todo.rs/.../todo.rs").
    if let Some(path) = params.path.as_deref() {
        let resolved = resolve_path_arg(ctx, path);
        if resolved.is_file() {
            return Ok(OutlineArgs {
                file: resolved.display().to_string(),
                json: false,
                max_items: None,
                path: None,
                context_json: context_json_path.map(|path| path.display().to_string()),
            });
        }
    }

    let file = outline_file_arg(params)?;
    Ok(OutlineArgs {
        file,
        json: false,
        max_items: None,
        path: resolved_root_string(ctx, params.path.as_deref()),
        context_json: context_json_path.map(|path| path.display().to_string()),
    })
}

pub(super) fn build_smart_args_and_query(
    params: &AgentGrepInput,
    ctx: &ToolContext,
    context_json_path: Option<&Path>,
) -> Result<(SmartArgs, SmartQuery)> {
    let terms = trace_or_smart_terms_owned(params)?;
    let query = parse_smart_query(&terms).map_err(|err| {
        anyhow::anyhow!(
            "{}\n\ntrace queries use a small DSL. Example:\n  agentgrep trace subject:auth_status relation:rendered support:ui",
            err
        )
    })?;
    let scope = resolved_search_scope(
        ctx,
        params.path.as_deref(),
        params.file.as_deref(),
        params.glob.as_deref(),
    );

    let args = SmartArgs {
        terms,
        json: false,
        max_files: params.max_files.unwrap_or(5),
        max_regions: params.max_regions.unwrap_or(6),
        full_region: parse_full_region_mode(params.full_region.as_deref())?,
        debug_plan: params.debug_plan.unwrap_or(false),
        debug_score: params.debug_score.unwrap_or(false),
        paths_only: params.paths_only.unwrap_or(false),
        path: scope.root,
        file_type: params.file_type.clone(),
        glob: scope.glob,
        hidden: params.hidden.unwrap_or(false),
        no_ignore: params.no_ignore.unwrap_or(false),
        context_json: context_json_path.map(|path| path.display().to_string()),
    };

    Ok((args, query))
}

pub(super) fn trace_or_smart_terms_owned(params: &AgentGrepInput) -> Result<Vec<String>> {
    if let Some(terms) = params.terms.as_ref().filter(|terms| !terms.is_empty()) {
        return Ok(terms.clone());
    }

    if params.mode == "smart"
        && let Some(query) = params.query.as_deref()
    {
        let split_terms: Vec<String> = query
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !split_terms.is_empty() {
            return Ok(split_terms);
        }
    }

    let field_hint = if params.mode == "smart" {
        "non-empty 'terms' or 'query'"
    } else {
        "non-empty 'terms'"
    };

    Err(anyhow::anyhow!(
        "agentgrep {} requires {}",
        params.mode,
        field_hint
    ))
}

fn outline_file_arg(params: &AgentGrepInput) -> Result<String> {
    params
        .file
        .clone()
        .or_else(|| params.query.clone())
        .or_else(|| {
            params
                .terms
                .as_ref()
                .and_then(|terms| terms.first().cloned())
        })
        .ok_or_else(|| {
            anyhow::anyhow!("agentgrep outline requires 'file' (or legacy 'query' / first term)")
        })
}

fn parse_full_region_mode(value: Option<&str>) -> Result<FullRegionMode> {
    match value.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(FullRegionMode::Auto),
        "always" => Ok(FullRegionMode::Always),
        "never" => Ok(FullRegionMode::Never),
        other => Err(anyhow::anyhow!(
            "agentgrep trace full_region must be one of: auto, always, never; got {other}"
        )),
    }
}

fn resolved_root_string(ctx: &ToolContext, path: Option<&str>) -> Option<String> {
    path.map(|path| resolve_path_arg(ctx, path).display().to_string())
}

/// Suggest existing directories near a path that does not exist.
///
/// MEASURED 2026-11-16: `agentgrep` failed 83 times with "search path does
/// not exist or is not a directory: ..." and the message named the bad path
/// but offered NO way forward, so the agent kept retrying the same argument.
/// The `read` tool already solves this shape ("File not found: X\nDid you
/// mean: Y, Z"), so this reuses that pattern instead of inventing one.
/// reference: C:\Users\WorkShop1\.jcode\research\2026-11-16-error-classes\error-classes-research.md
///   (class B1: the ready-made solution already in this project)
pub(super) fn find_similar_dirs(path: &Path) -> Vec<String> {
    let mut suggestions = Vec::new();
    let mut current = path.parent();
    // Walk up to the first existing ancestor, then list its subdirectories.
    while let Some(dir) = current {
        if dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    if entry.path().is_dir() {
                        suggestions.push(entry.path().display().to_string());
                        if suggestions.len() >= 5 {
                            return suggestions;
                        }
                    }
                }
            }
            if !suggestions.is_empty() {
                return suggestions;
            }
        }
        current = dir.parent();
    }
    suggestions
}

pub(super) fn resolve_search_root(ctx: &ToolContext, path: Option<&str>) -> Result<PathBuf> {
    let root = path
        .map(PathBuf::from)
        .or_else(|| ctx.working_dir.clone())
        .ok_or_else(|| anyhow::anyhow!("agentgrep requires a session working directory"))?;
    // The search root must be an existing DIRECTORY: the agentgrep crate runs
    // ripgrep with `current_dir(root)`, so a root that does not exist (or that
    // is a file) fails with the raw OS error 267 ("Неверно задано имя папки" =
    // "the directory name is invalid"), which reads like a broken tool rather
    // than a bad argument.
    //
    // MEASURED 2026-11-16: 83 failures of this shape, and the message named
    // the bad path but offered no way forward - the agent retried the same
    // argument. The hint below is the same "Did you mean" shape the `read`
    // tool already uses, applied to the nearest existing directory.
    if !root.is_dir() {
        let mut message = format!(
            "agentgrep search path does not exist or is not a directory: {}",
            root.display()
        );
        let suggestions = find_similar_dirs(&root);
        if !suggestions.is_empty() {
            message.push_str(&format!("\nDid you mean: {}", suggestions.join(", ")));
        } else if let Some(parent) = root.parent().filter(|p| p.is_dir()) {
            message.push_str(&format!(
                "\nThe closest existing directory is: {}",
                parent.display()
            ));
        }
        return Err(anyhow::anyhow!(message));
    }
    Ok(root)
}

pub(super) fn summarize_agentgrep_request(
    params: &AgentGrepInput,
    ctx: &ToolContext,
    context_json_path: Option<&Path>,
) -> String {
    let mut parts = vec![format!("mode={}", params.mode)];
    if let Some(query) = params.query.as_deref() {
        parts.push(format!("query={}", util::truncate_str(query, 80)));
    }
    if let Some(file) = params.file.as_deref() {
        parts.push(format!("file={file}"));
    }
    if let Some(terms) = params.terms.as_ref() {
        parts.push(format!(
            "terms={}",
            util::truncate_str(&terms.join(" "), 80)
        ));
    }
    if let Some(path) = resolved_root_string(ctx, params.path.as_deref()) {
        parts.push(format!("root={path}"));
    }
    if let Some(glob) = normalized_agentgrep_glob(params.glob.as_deref()) {
        parts.push(format!("glob={glob}"));
    }
    if let Some(file_type) = params.file_type.as_deref() {
        parts.push(format!("type={file_type}"));
    }
    if params.paths_only.unwrap_or(false) {
        parts.push("paths_only=true".to_string());
    }
    if context_json_path.is_some() {
        parts.push("context_json=true".to_string());
    }
    parts.join(" ")
}
