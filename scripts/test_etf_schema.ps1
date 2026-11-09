# Deterministic schema test for the Jcode adapter's Entire Transcript Format v1 output.
#
# reference: https://raw.githubusercontent.com/entireio/cli/main/transcript/README.md
# reference: https://docs.entire.io/agents/agent-integration-protocol/commands
#
# Usage: pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/test_etf_schema.ps1
#
# The test has TWO halves:
#
#   1. NEGATIVE CONTROL. A synthetic transcript that violates the spec in
#      each way is fed to the same checker the live transcripts go through.
#      The checker MUST reject every one of them. A checker that accepts a
#      broken transcript is worthless, so the control runs first and its
#      failure stops the run.
#
#   2. LIVE TRANSCRIPTS. Real Jcode sessions are converted by the adapter and
#      every emitted line is checked against the spec: v is the integer 1,
#      type is "user" or "assistant", content is an array of blocks or a
#      string or null, the known scalar fields have the right JSON type, and
#      the v1 quirks hold (a user text block carries NO "type" field, a
#      reasoning block is dropped).
#
# The content check is closed: content that is neither an array, a string,
# nor null is a FAILURE. The earlier version of this test only checked that
# content was not $null, so a spec-violating non-array non-string content
# passed it.

$ErrorActionPreference = "Stop"

$adapter = $env:ENTIRE_JCODE_ADAPTER
if (-not $adapter) {
    $adapter = "C:\Users\WorkShop1\proj\entire-agent-jcode\entire-agent-jcode.py"
}
$sessions = $env:JCODE_HOME
if (-not $sessions) {
    $sessions = "$env:USERPROFILE\.jcode\sessions"
} else {
    $sessions = "$sessions\sessions"
}
$maxLineBytes = 1MB

if (-not (Test-Path $adapter)) {
    Write-Output "RESULT: FAIL (adapter not found: $adapter)"
    exit 1
}

$failed = $false

function Invoke-Adapter([string]$arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "python"
    $psi.Arguments = $arguments
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    # The adapter writes UTF-8; the default console codepage would mangle
    # every non-ASCII character in the transcript.
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $proc = [System.Diagnostics.Process]::Start($psi)
    $out = $proc.StandardOutput.ReadToEnd()
    $err = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()
    return @{ Out = $out; Err = $err; Code = $proc.ExitCode }
}

function Test-JsonInteger($value) {
    # PowerShell's ConvertFrom-Json yields Int64 for a JSON integer, not
    # Int32, and Double for a JSON float. A JSON boolean is [bool] and is
    # not an integer.
    return ($value -is [int]) -or ($value -is [long])
}

# ---------------------------------------------------------------------------
# The checker. It reports the problems it found, one entry per line.
# ---------------------------------------------------------------------------

function Get-TranscriptProblems([string[]]$lines) {
    $problems = New-Object System.Collections.Generic.List[string]
    $maxLine = 0
    $toolUses = 0
    $toolResults = 0
    $userTextBlocks = 0
    $userTextWithType = 0
    $reasoning = 0
    $types = @{}

    foreach ($line in $lines) {
        $len = [System.Text.Encoding]::UTF8.GetByteCount($line)
        if ($len -gt $maxLine) { $maxLine = $len }
        if ($len -gt $maxLineBytes) {
            $problems.Add("line of $len bytes exceeds the 1 MB Brain line budget")
        }
        try {
            $obj = $line | ConvertFrom-Json
        } catch {
            $problems.Add("not valid JSON")
            continue
        }
        if ($null -eq $obj -or $obj -isnot [psobject]) {
            $problems.Add("not a JSON object")
            continue
        }
        # v must be the INTEGER 1. A string "1" or a float 1.0 is not v1.
        if (-not (Test-JsonInteger $obj.v) -or $obj.v -ne 1) {
            $problems.Add("v is not the integer 1 (got '$($obj.v)')")
        }
        if ($obj.type -ne "user" -and $obj.type -ne "assistant") {
            $problems.Add("type is not user/assistant (got '$($obj.type)')")
        }
        # The known scalar fields must have the right JSON type. A null
        # optional scalar is absence, not a wrong type.
        foreach ($key in @("agent", "cli_version", "id", "model")) {
            $field = $obj.$key
            if ($null -ne $field -and $field -isnot [string]) {
                $problems.Add("$key is not a string (got '$($field)')")
            }
        }
        foreach ($key in @("input_tokens", "output_tokens", "cache_read_tokens",
                          "cache_write_tokens", "cache_write_5m_tokens",
                          "cache_write_1h_tokens", "reasoning_tokens")) {
            $field = $obj.$key
            if ($null -ne $field -and -not (Test-JsonInteger $field)) {
                $problems.Add("$key is not an integer (got '$($field)')")
            }
        }
        # content must be an array of blocks, a string, or null. Anything else
        # is a failure: the earlier checker only tested for $null.
        if ($null -ne $obj.content -and $obj.content -isnot [array] -and $obj.content -isnot [string]) {
            $problems.Add("content is neither an array, a string, nor null")
        }
        # A line whose type is missing must not be counted under a null key:
        # the count table is keyed by the type, and a null key is not an index.
        if ($null -ne $obj.type) {
            $types[$obj.type] = 1 + [int]$types[$obj.type]
        }
        foreach ($block in @($obj.content)) {
            if ($null -eq $block) { continue }
            if ($block.type -eq "tool_use") {
                $toolUses++
                if ($null -ne $block.result) {
                    $toolResults++
                    if ($block.result.status -ne "success" -and $block.result.status -ne "error") {
                        $problems.Add("tool_use result has a bad status '$($block.result.status)'")
                    }
                }
            } elseif ($block.type -eq "reasoning") {
                $reasoning++
            } elseif ($null -eq $block.type) {
                # A user text block in ETF v1 carries NO "type" field.
                $userTextBlocks++
            } elseif ($block.type -eq "text" -and $obj.type -eq "user") {
                # A user text block WITH a type field is the v1 quirk violation.
                $userTextWithType++
            }
        }
    }
    if ($reasoning -gt 0) { $problems.Add("$reasoning reasoning blocks were not dropped") }
    if ($userTextWithType -gt 0) { $problems.Add("$userTextWithType user text blocks carry a type field") }
    return @{ Problems = $problems; MaxLine = $maxLine; ToolUses = $toolUses;
               ToolResults = $toolResults; UserTextBlocks = $userTextBlocks;
               Reasoning = $reasoning; Types = $types }
}

# ---------------------------------------------------------------------------
# 1. NEGATIVE CONTROL
# ---------------------------------------------------------------------------

Write-Output "=== negative control"

$controls = @(
    @{ Name = "not JSON at all";            Line = "not json at all" }
    @{ Name = "truncated JSON";             Line = '{"v":1,"type":"user"' }
    @{ Name = "v is 2";                     Line = '{"v":2,"type":"user","content":[]}' }
    @{ Name = "v is a string";              Line = '{"v":"1","type":"user","content":[]}' }
    @{ Name = "v is a float";                Line = '{"v":1.0,"type":"user","content":[]}' }
    @{ Name = "v is a boolean";              Line = '{"v":true,"type":"user","content":[]}' }
    @{ Name = "v is missing";               Line = '{"type":"user","content":[]}' }
    @{ Name = "type is missing";            Line = '{"v":1,"content":[]}' }
    @{ Name = "type is a number";           Line = '{"v":1,"type":7,"content":[]}' }
    @{ Name = "content is a number";        Line = '{"v":1,"type":"user","content":42}' }
    @{ Name = "content is an object";       Line = '{"v":1,"type":"user","content":{"text":"x"}}' }
    @{ Name = "content is a boolean";       Line = '{"v":1,"type":"user","content":true}' }
    @{ Name = "agent is a number";          Line = '{"v":1,"type":"user","agent":42,"content":[]}' }
    @{ Name = "input_tokens is a string";   Line = '{"v":1,"type":"user","input_tokens":"x","content":[]}' }
    @{ Name = "user text carries a type";   Line = '{"v":1,"type":"user","content":[{"type":"text","text":"x"}]}' }
    @{ Name = "reasoning block kept";       Line = '{"v":1,"type":"assistant","content":[{"type":"reasoning","text":"x"}]}' }
    @{ Name = "tool result status is bad";  Line = '{"v":1,"type":"assistant","content":[{"type":"tool_use","name":"t","input":{},"result":{"output":"o","status":"weird"}}]}' }
)

$controlFailures = 0
foreach ($control in $controls) {
    $result = Get-TranscriptProblems @($control.Line)
    if ($result.Problems.Count -eq 0) {
        Write-Output "    FAIL the checker ACCEPTED: $($control.Name)"
        $controlFailures++
    } else {
        Write-Output "    ok  rejected: $($control.Name) -> $($result.Problems[0])"
    }
}
# The good cases must NOT be rejected, or the checker rejects everything.
$good = @(
    @{ Name = "v1 user line";  Line = '{"v":1,"type":"user","agent":"jcode","content":[{"text":"x"}]}' }
    @{ Name = "v1 assistant";  Line = '{"v":1,"type":"assistant","agent":"jcode","cli_version":"0.13.1","content":[{"type":"text","text":"x"}]}' }
    @{ Name = "string content"; Line = '{"v":1,"type":"assistant","content":"hello"}' }
    @{ Name = "null content";  Line = '{"v":1,"type":"assistant","content":null}' }
    @{ Name = "null optionals"; Line = '{"v":1,"type":"user","agent":null,"content":[{"text":"x"}]}' }
    @{ Name = "non-string ts"; Line = '{"v":1,"type":"user","ts":123,"content":[{"text":"x"}]}' }
    @{ Name = "token counts"; Line = '{"v":1,"type":"assistant","input_tokens":10,"output_tokens":20,"cache_read_tokens":30,"cache_write_tokens":40,"content":[{"type":"text","text":"x"}]}' }
)
foreach ($case in $good) {
    $result = Get-TranscriptProblems @($case.Line)
    if ($result.Problems.Count -gt 0) {
        Write-Output "    FAIL the checker REJECTED a valid line: $($case.Name) -> $($result.Problems[0])"
        $controlFailures++
    } else {
        Write-Output "    ok  accepted: $($case.Name)"
    }
}

if ($controlFailures -gt 0) {
    Write-Output ""
    Write-Output "RESULT: FAIL (the checker itself is wrong: $controlFailures control failures)"
    exit 1
}
Write-Output "    checker agrees with the spec on $($controls.Count) broken and $($good.Count) valid lines"
Write-Output ""

# ---------------------------------------------------------------------------
# 2. LIVE TRANSCRIPTS
# ---------------------------------------------------------------------------

$targets = @()
if (Test-Path $sessions) {
    $all = @(Get-ChildItem -Path $sessions -Filter "session_*.json" -File |
             Sort-Object Length -Descending | Select-Object -First 3)
    $targets = $all | ForEach-Object { $_.FullName }
}
if ($targets.Count -eq 0) {
    Write-Output "RESULT: FAIL (no sessions found under $sessions)"
    exit 1
}

foreach ($target in $targets) {
    $bytes = [System.IO.File]::ReadAllBytes($target)
    $doc = [System.Text.Encoding]::UTF8.GetString($bytes) | ConvertFrom-Json
    $messageCount = @($doc.messages).Count

    $run = Invoke-Adapter "`"$adapter`" read-transcript --session-ref `"$target`""

    Write-Output "=== $(Split-Path $target -Leaf)"
    Write-Output "    session bytes : $($bytes.Length)"
    Write-Output "    messages      : $messageCount"
    Write-Output "    exit code     : $($run.Code)"
    if ($run.Err.Trim()) { Write-Output "    stderr        : $($run.Err.Trim())" }

    if ($run.Code -ne 0) { Write-Output "    FAIL: nonzero exit"; $failed = $true; continue }

    $text = $run.Out
    $lines = @($text -split "`n" | Where-Object { $_.Trim() -ne "" })
    Write-Output "    etf lines     : $($lines.Count)"

    if ($messageCount -eq 0) {
        Write-Output "    (session has no messages; no lines expected)"
        Write-Output ""
        continue
    }

    $result = Get-TranscriptProblems $lines
    Write-Output "    max line bytes: $($result.MaxLine)"
    Write-Output "    line types     : $((($result.Types.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ', '))"
    Write-Output "    tool_use blocks: $($result.ToolUses) (with inline result: $($result.ToolResults))"
    Write-Output "    user text blks : $($result.UserTextBlocks)"
    Write-Output "    reasoning blks : $($result.Reasoning) (must be 0)"

    foreach ($problem in $result.Problems) {
        Write-Output "    FAIL: $problem"
        $failed = $true
    }
    if ($lines.Count -eq 0) { Write-Output "    FAIL: no lines emitted"; $failed = $true }
    if ($lines.Count -gt $messageCount) { Write-Output "    FAIL: more lines than messages"; $failed = $true }

    # The compact-transcript subcommand must round-trip the same bytes.
    $run2 = Invoke-Adapter "`"$adapter`" compact-transcript --session-ref `"$target`""
    if ($run2.Code -ne 0) {
        Write-Output "    FAIL: compact-transcript exit $($run2.Code)"
        $failed = $true
    } else {
        $resp = $run2.Out | ConvertFrom-Json
        $decoded = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($resp.transcript))
        # read-transcript appends a trailing newline when the payload lacks one;
        # the compact payload is the transcript bytes verbatim.
        $same = ($decoded.TrimEnd("`n") -eq $text.TrimEnd("`n"))
        Write-Output "    compact matches read-transcript: $same"
        if (-not $same) { Write-Output "    FAIL: compact-transcript bytes differ"; $failed = $true }
    }
    Write-Output ""
}

# ---------------------------------------------------------------------------
# 3. The adapter must REFUSE a broken transcript
# ---------------------------------------------------------------------------

Write-Output "=== refusal"
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("etf-nc-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $scratch -Force | Out-Null
try {
    $garbage = Join-Path $scratch "garbage.json"
    [System.IO.File]::WriteAllBytes($garbage, [byte[]]@(0x6e, 0x6f, 0x74, 0x20, 0x6a, 0x73, 0x6f, 0x6e))
    $refuse = Invoke-Adapter "`"$adapter`" read-transcript --session-ref `"$garbage`""
    if ($refuse.Code -eq 0) {
        Write-Output "    FAIL: the adapter returned a transcript for a garbage file"
        $failed = $true
    } else {
        Write-Output "    ok  the adapter refused a garbage file (exit $($refuse.Code))"
    }
    $missing = Invoke-Adapter "`"$adapter`" read-transcript --session-ref `"$scratch\does-not-exist.jsonl`""
    if ($missing.Code -eq 0) {
        Write-Output "    FAIL: the adapter returned a transcript for a missing session"
        $failed = $true
    } else {
        Write-Output "    ok  the adapter refused a missing session (exit $($missing.Code))"
    }
} finally {
    Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
}
Write-Output ""

if ($failed) { Write-Output "RESULT: FAIL"; exit 1 }
Write-Output "RESULT: PASS"
exit 0
