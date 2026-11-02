---
name: executing-plans
description: Use when executing an implementation plan inline in this session - the ledger is your memory
---

# Executing Plans (inline)

Execute the plan yourself, task by task. The plan already did the
thinking - execute it exactly, prove each step with a test you watched
fail and then pass, and leave a record that survives your own forgetting
(compaction will erase your conversational memory; the ledger will not).

**Narration:** between tool calls, at most one short line - the ledger
and tool results carry the record.

## The ledger

`.modron/sdd/<plan-slug>/progress.md`, first line names the plan file:

    # <plan-slug> — executing docs/plans/....md
    Task 1: in progress
    Task 1: done — <one line on what landed>
    Ruling: <what you decided> — <why> — <what it costs if wrong>
    Task 2: in progress

Before ANY dispatch or edit, if a ledger exists for this plan, read it
and resume from the first unfinished task. Never re-do completed tasks.
Update it after every task, ruling, and review round.

## Continuous execution

Do not pause to check in between tasks. **Rulings, not stalls:**
conflicts, ambiguities, and plan defects get DECIDED, recorded as a
`Ruling:` line, and executed. A session parked on a question costs your
partner their whole day; a wrong ruling costs rework they can see and
undo in the ledger.

Only four things stop you: an irreversible/destructive operation; a
security-sensitive action; a side effect outside this worktree that
norms say to ask about (merge, push to a shared branch, publish); and a
plan so broken every path forward is a guess.

## Per task

Read the task's brief from the plan -> write the failing test, watch it
fail (see `test-driven-development`) -> minimal implementation -> tests
pass -> commit. At the end: run the full check suite (`diagnostics` or
the project's verify command) and one `requesting-code-review` round on
the whole branch.

Adapted from Jesse Vincent's superpowers (MIT): github.com/obra/superpowers
