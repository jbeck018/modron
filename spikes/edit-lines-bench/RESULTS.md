# H6b: edit_lines vs edit, measured with real model calls

Packet H6b (LEDGER H6). Question: should `edit_lines` (hash-anchored line
edits plus `read` line hashes) replace the default `edit` tool? Answer
below: no, keep `edit` as the default and `edit_lines` opt-in.

## Setup

- Date: 2026-11-16. Model requested: `glm-4.6` via provider `zai` (Z.ai
  coding plan, reached through the loopback forwarder). The API reports the
  served model as **`glm-5.3-flash`** for that request (checked with a direct
  chat call: `"model":"glm-5.3-flash"`). Read the numbers as glm-5.3-flash.
- Harness: `modron eval --provider zai --models glm-4.6 --suite edit
  --edit-tool <edit|edit_lines> --delay-ms 1500`, approve-all, 16 turns max,
  300k token budget per task. Each task runs in a fresh workspace under
  $TMPDIR; the verifier is a shell `cmp` against the exact expected bytes.
- Runs: 3 rounds per arm, 12 tasks per round (72 task runs). Both arms ran
  concurrently (two processes), rounds sequential.
- Binary: `cargo build -p modron-cli --bin modron` from this branch.
- Raw records: /tmp/claude-0/bench-results/raw/*.jsonl (6 files, 72 rows).
  Repo copies: per-task.tsv and summary.txt in this directory.

## The switch

- `modron eval --edit-tool edit|edit_lines` (default `edit`).
- `modron_tools::prefer_edit_lines` (copy of the registry: `edit` removed,
  `edit_lines` in the prompt schema, read ledger shared). Applied only when
  the arm is `edit_lines`. Production registries are untouched.
- Per-row metrics: edit calls, edit errors, edit retries (edit calls made
  after an earlier edit error in the same task), first three error samples.

## Results (3 rounds x 12 tasks each)

| arm        | passed | edit calls (r1/r2/r3) | edit errors | retries | mean tokens/task (sd) | mean ms/task (sd) |
|------------|--------|-----------------------|-------------|---------|-----------------------|-------------------|
| edit       | 36/36  | 14 / 15 / 13          | 1 / 1 / 1   | 1/1/1   | 17099 (2054)          | 11863 (1836)      |
| edit_lines | 33/36  | 13 / 17 / 17          | 2 / 4 / 5   | 2/6/7   | 19073 (694)           | 13495 (555)       |

Per-round passes: edit 12/12, 12/12, 12/12; edit_lines 11/12, 11/12, 11/12.
Every edit_lines miss was edit-10-blank-lines. Its record in run 1 shows one
edit call with no tool error, so the miss was a wrong result, not a tool
failure. It was not diagnosed further.

Tool calls per task: edit 3.3-3.6, edit_lines 3.9-4.3.

## What the failures looked like

edit (1 error per round, all the same kind):
  `edit: read-before-edit gate: read `crlf.txt` first -- edits to files the session has not read are blocked`

edit_lines (2-5 errors per round), three kinds:
  `edit_lines: invalid arguments: missing required argument `start_line``
  `edit_lines: `expected_hashes` has 3 entries for 2 line(s) in 7..=8; pass exactly one hash per line`
  `edit_lines: hash mismatch in .../config.ini: the f...` (stale anchor after an earlier edit in the same task)

Interpretation: the hash anchor works as designed (the stale-hash refusals
are the point of the tool), but the argument shape is easy to get wrong:
the model sometimes omits start_line or sends a hash list that does not
match the range. Each error costs a turn, so edit_lines used more turns,
tokens and time for the same passes.

## Variance

With 3 rounds and 12 tasks, the pass counts are stable within each arm
(36/36 vs 33/36, every round identical). Token and time means differ by
about 12 percent and 14 percent. The spreads (sd about 0.7k to 2k tokens)
are not large next to that gap, but three rounds are too few for a
significance claim.

## Threats to validity

- One model family (GLM via the Z.ai coding plan), and the served model is
  glm-5.3-flash, not the id requested. Results may not transfer to other
  models or to Claude.
- Single provider, subscription rate limits (1.5 s pause between tasks; no
  429s observed in the logs). Two processes ran at once.
- Suite: 12 small tasks written for this comparison, each with a byte-exact
  verifier. A reference-solution check passes all 12 verifiers. The suite
  favours small edits; it does not cover large refactors or long sessions.
- The edit_lines arm removes `edit` from the registry, but the system prompt
  may still mention `edit` in general guidance. Not audited.
- edit-10 failures in the edit_lines arm are not diagnosed.
- The mini-SWE suite was not run; it has few edit-heavy tasks.

## Recommendation

Keep `edit` as the default and keep `edit_lines` opt-in. On this evidence
edit_lines is not better: 3 misses in 36 runs, more edit errors, and
about 12 percent more tokens and 14 percent more time per task. The
measured gap is small and comes from argument-shape errors, which could be
fixed. Before any default change, re-run this suite on a second model
family and check whether the argument errors drop. Offering both tools
together is not supported by this data either: it adds a choice the model
then has to get right.

## Reproduce

    TMPDIR=/tmp/claude-0/bench-work/tmp ZAI_BASE_URL=http://127.0.0.1:18089/api/coding/paas/v4 ZAI_API_KEY=... \
      modron --provider zai eval --models glm-4.6 --suite edit --edit-tool edit_lines --delay-ms 1500

## Addendum: OpenAI models (2026-11-16)

Same 12 tasks, 3 rounds per arm, through `--provider openai-responses`
(the chat-completions provider refused GPT-5 models at the time; fixed
in the same change set). Arms ran concurrently.

| model | arm | passed | tok/task | s/task | edit calls | tool errors |
|---|---|---|---|---|---|---|
| gpt-5.5 | edit | 36/36 | 13586 | 7.8 | 38 | 1 |
| gpt-5.5 | edit_lines | 36/36 | 13644 | 7.6 | 33 | 0 |
| gpt-5.4-mini | edit | 36/36 | 16078 | 5.7 | 34 | 2 |
| gpt-5.4-mini | edit_lines | 34/36 | 13247 | 4.8 | 30 | 1 |

Reading: on the strong model the two tools tie (edit_lines made fewer
calls and no errors; tokens and time equal). On the small model
edit_lines used about 18 percent fewer tokens but failed
edit-10-blank-lines twice, the same task the Z.ai model failed every
round. That task fails only with edit_lines and only on weaker models;
it is not diagnosed (tool behaviour around blank lines, or the task).
The recommendation stands: edit stays the default, edit_lines opt-in,
and edit-10 is the thing to look at before revisiting.


## Diagnosis of edit-10-blank-lines

Cause: model error in new_text, not a tool defect. The tool applies exactly the
lines it is given; pass or fail depends on whether new_text starts with a newline.

Evidence (blank.txt: title, blank, blank, section one, body a, blank, blank,
section two, body b). Failing gpt-5.4-mini call (session 14588d62, others alike):
  start_line=6 end_line=9 expected_hashes=[c8d,c8d,ce9,613]
  new_text="divider\n\nsection two\nbody B"
Tool result: 4 lines replaced by 4; file is body a / divider / blank / section two,
so the blank before divider is lost. Passing call: same range, new_text starting
with a newline. Other failures: start 5 with "body a\ndivider..." (one blank lost),
and glm-4.6 start 5 that doubled the blanks. Eval rows show edit_errors=0 each time.
In every logged run, a leading-newline call passed and every other call failed.
The default edit tool has no line arithmetic and passes the same task byte-for-byte.

Changes: tests only (edit_lines.rs and edit.rs). Logged calls replayed verbatim
assert their exact bytes; newline convention test added; default-edit blank test
added. Tool behavior, schema and descriptions unchanged. A description rewrite
and a "(blank)" rail marker were tried and reverted: no gain measured.

Rerun numbers (edit-10, edit_lines):
  baseline gpt-5.4-mini 10/15 (+ control 3/6 later); baseline glm-4.6 2/3
  variants (discarded): gpt-5.4-mini 5/15 and 2/12
  final binary: gpt-5.4-mini full suite 11/12 (edit-10 fails); glm-4.6 12/12

Recommendation: no change. Keep edit as default, edit_lines opt-in. Weaker models
miscount blank lines because edit_lines makes them re-send unchanged lines;
an insert-after operation would avoid that, but it is not implemented.
