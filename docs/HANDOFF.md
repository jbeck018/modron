# Handoff between hosts

A handoff moves one session from one host to another: its working tree,
its session log and the right to run it. One host owns a session at a time.

## What travels and what does not

Travels (git carrier, the default):

- tracked changes, deleted tracked files, untracked files that are not
  ignored, executable bits and symlinks, as one commit on
  `refs/heads/modron/handoff/<id>` (parent: the exporter's HEAD);
- the session bundle, a tar of exactly three store files: `<id>.jsonl`
  (the log), `<id>.meta.json` (title) and `<id>.rewind` (rewind marker),
  as one commit on `refs/heads/modron/handoff/<id>-session`.

Does not travel:

- ignored files (`git status --porcelain --ignored` lists how many were
  left behind; export prints the count);
- provider credentials, env files and anything else in the store
  directory: the bundle is an allow-list, never a glob;
- the HandedOff line: it is written only to the exporting host's log.

Export never moves the user's index, HEAD or branch: the tree is built from
a temporary index (`GIT_INDEX_FILE`) and only the two refs are written.

## Commands

    modron handoff export [--session ID] [--remote origin] [--no-push]
    modron handoff import <ref-or-session-id> [--remote origin] [--force]

Export pushes both refs (forced, so each handoff replaces the previous one)
and only then appends `HandedOff` to the local log. Without a remote it
fails unless `--no-push` is given.

Import fetches from `--remote` when that remote exists, checks the tree out
into the working directory with `read-tree` and `checkout-index` (the branch
is not switched), removes files the handoff deleted, and restores the log.
It prints `modron --resume=<id>` when the session is runnable here.

## Ownership

- Export appends `HandedOff { to, at, carrier_ref }` as the last line of
  the exporting host's log. While that is the last line, that host refuses
  runs, compaction and `!` commands on the session, naming the import
  command.
- Import compares logs by lines. Let the local log (minus a trailing
  HandedOff line) be the base. The incoming log is accepted when it is
  identical to the base (no change), or when it is a strict extension of
  the base (the log is replaced, which clears the handoff). Any other
  incoming log is refused with both lengths. A refused import changes
  nothing.
- There is no lock. The prefix rule decides who is right; a diverged log is
  refused, never merged.

## Checks before a write

- The working tree must be clean (`git status --porcelain` empty) unless
  `--force`. Files that would be overwritten (ignored or untracked files at
  paths the handoff has) also need `--force`.
- The log decision runs before any file is touched.

## Limits

- Live handoff during a run is not supported: export a session that is idle.
  Export does not detect a running `modron serve`; the engine refusal is the
  guard, because a running host reads the HandedOff line before its next run.
- Diverged trees are not merged. Two hosts editing the same session in
  parallel are out of scope.
- The furrow carrier is not built. `[handoff] carrier = "furrow"` fails with
  a "not built yet" message (see docs/CONFIG.md).
- Provider credentials do not move; each host signs in on its own.
