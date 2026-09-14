# Autonomous blocker resolution policy

> Status: implemented in `jcode-base`.
> Scope: planning guidance and deterministic decision primitives. This policy does not replace execution safety or grant permissions.

## Why this exists

A failed command, unavailable website, missing tool, or broken build is not automatically a reason to stop. The agent should preserve the user's objective, diagnose the cause, find a permitted equivalent route, apply the smallest useful step, and verify the requested result.

The policy is intentionally bounded. It prevents two opposite failures:

- premature stopping at the first error;
- unbounded retries or unsafe escalation of privileges.

## Evidence from external practice

The implementation follows converging patterns from primary documentation and research:

- [Claude Code permissions](https://docs.anthropic.com/en/docs/claude-code/permissions) separates read-only work from edits, external fetches, and commands, and enforces deny/ask/allow outside the model's prompt. Its auto mode uses background safety checks rather than treating model intent as authorization.
- [OpenAI Agents SDK guardrails](https://openai.github.io/openai-agents-python/guardrails/) places checks at tool boundaries and supports blocking execution before a side effect. This is why the policy is advisory until the existing tool and command gates inspect the real call.
- [OpenAI Agents SDK human-in-the-loop](https://openai.github.io/openai-agents-python/human_in_the_loop/) persists interrupted runs and resumes them after approval. This supports escalating a specific action instead of abandoning the whole mission.
- [LangGraph persistence](https://docs.langchain.com/oss/python/langgraph/durable-execution) treats checkpoints as necessary for interruption, recovery, and fault tolerance. Jcode already has durable sessions, todos, gate observations, and mission continuation, so the policy records concise blocker state rather than relying on transient model context.
- [ReAct](https://arxiv.org/abs/2210.03629) provides evidence that interleaving reasoning with actions helps update plans and handle exceptions. Jcode applies the useful part, observable hypothesis/action/result loops, without requesting or storing private chain-of-thought.
- Jcode's existing `mission_continuation.md`, todo quality gates, `jcode-command-risk`, hooks, and `safety` modules already enforce persistence, evidence checks, command blast-radius review, and permission boundaries. The new layer composes with them rather than duplicating or weakening them.

## Policy algorithm

1. Preserve the literal objective, user constraints, and required deliverables.
2. Update todo and create a concise blocker record with a category, symptom, attempted route, result, and next step.
3. Reproduce with a bounded read-only check where possible.
4. Classify the blocker: access/auth, network, toolchain/dependency, source/configuration, context, evidence, external policy, safety, or unknown.
5. Choose the smallest evidence-producing next action. For a research or access failure, use an official API, archive, mirror, local source checkout, or more authoritative equivalent when appropriate. Explain source substitution briefly in the final report.
6. Do not repeat an identical retry. A retry must change a tested variable or follow new evidence. A reversible change must have an acceptance check.
7. Allow only in-scope read-only or reversible-local planning steps. The actual tool call still passes through command-risk, hooks, safety, credentials, and user-confirmation checks.
8. Require permission for external side effects, credential changes, destructive actions, communication, deployment, financial/account actions, or scope expansion. Protected/catastrophic targets escalate and are never bypassed by this policy.
9. Verify against the real public or end-user acceptance path where available, plus relevant edge cases and integration boundaries. Confidence is evidence-calibrated. A requested 96% target cannot justify inflated certainty.
10. Continue while a safe, bounded, evidence-producing route exists. Escalate only when the route is exhausted, unsafe, outside scope, requires unavailable external state, or needs a specific user decision.

## Implemented primitives

`crates/jcode-base/src/blocker_policy.rs` provides serializable types for:

- blocker cause and lifecycle;
- candidate action kind and planning risk;
- evidence strength;
- concise attempt records;
- bounded policy limits;
- `UnblockingPolicy::evaluate`, which returns `Continue`, `RequestPermission`, `Reject`, or `Escalate`.

The system prompt includes the same operational guidance through `AUTONOMOUS_BLOCKER_UNBLOCKING_PROMPT`. The prompt explicitly says that it is subordinate to higher-priority instructions and never grants permission.

## Conflict resolution

Priority is:

1. system and developer instructions;
2. explicit safety, command-risk, permission, credential, and tool gates;
3. explicit user objective and constraints;
4. this recovery policy;
5. model preference for a particular route.

The policy can increase persistence and research breadth. It cannot authorize a prohibited operation, override a denied tool call, reveal a secret, reset a password, or convert a user request for research into an unrelated side effect.

## Audit format

Use a short, observable trace rather than private reasoning:

```text
blocker: Reddit JSON returned HTTP 403
hypothesis: service access policy, not local proxy
alternative: Arctic Shift full-text API
action: fetch post and comment tree read-only
result: full post text retrieved
limitation: self-report is anecdotal, not controlled benchmark data
next_step: compare with official docs and independent benchmarks
```

This trace belongs in a todo, artifact, or final status report and should not include credentials or hidden chain-of-thought.
