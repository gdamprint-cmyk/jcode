# Entire external-agent adapter for Jcode

The Entire CLI (https://docs.entire.io) captures an agent's session as an
"Entire Transcript Format v1" (ETF v1) transcript and turns it into
checkpoints. It does that by driving an EXTERNAL AGENT PLUGIN: a program the
CLI executes with a fixed set of subcommands.

This directory holds that plugin for Jcode.

reference: https://github.com/entireio/external-agents/blob/main/agents/entire-agent-hermes/
reference: https://docs.entire.io/agents/agent-integration-protocol/commands
reference: https://github.com/entireio/cli/blob/main/cmd/entire/cli/agent/external/types.go
reference: https://raw.githubusercontent.com/entireio/cli/main/transcript/README.md

## What is here

| file | what it is |
| --- | --- |
| `entire-agent-jcode.py` | The adapter. The Entire CLI runs this. |
| `entire-agent-jcode-hook.py` | The lifecycle hook. Jcode runs this on session/turn events; it forwards them to `entire hooks jcode <name>`. |
| `test_transcript_refusal.py` | Regression tests: the adapter must REFUSE to substitute data for a transcript. |
| `test_shrink_etf_line.py` | Regression tests: trimming an oversized ETF line must never lose a payload. |
| `neg_control_old_adapter.py` | Negative control: the new tests must FAIL against the old adapter. |
| `neg_control_tiny_payload.py` | Negative control: the adapter version that empties a tiny payload must still empty it. |
| `old_adapter_snapshot.py` | Vendored snapshot of the adapter at `fc457a7`, for the negative control above. |
| `tiny_payload_snapshot.py` | Vendored snapshot of the adapter at `05cddf4`, for the negative control above. |

The adapter and the hook are ported verbatim from the external adapter
repository `entire-agent-jcode` at `f4573ad`. The only edit is the provenance
header at the top of each file.

## Running the tests

```
python scripts/entire/test_transcript_refusal.py
python scripts/entire/test_shrink_etf_line.py
python scripts/entire/neg_control_old_adapter.py
python scripts/entire/neg_control_tiny_payload.py
```

The ETF v1 schema test lives one level up, because it also checks REAL
sessions on this host:

```
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/test_etf_schema.ps1
```

It runs its own embedded negative control FIRST (19 malformed transcripts that
must be rejected for the expected reason, 7 valid lines that must be
accepted). If the control fails, the test aborts before any live check.

## Installing the agent for a repository

The adapter must be on PATH under the name `entire-agent-jcode`, and the
repository must have the agent registered:

```
entire agent add jcode
```

The hook is installed into Jcode's own config by the adapter:

```
python scripts/entire/entire-agent-jcode.py install-hooks
```

The hook forwards to `entire hooks jcode <name>`. The Entire CLI resolves an
external agent PER REPOSITORY, so the hook runs the `entire` subprocess in an
Entire-enabled repository rather than in the session's cwd.
`ENTIRE_JCODE_REPO_ROOT` overrides which one.
