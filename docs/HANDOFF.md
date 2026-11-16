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

- ignored files (git carrier only: `git status --porcelain --ignored` lists
  how many were left behind, and export prints the count; the furrow carrier
  carries them);
- provider credentials, env files and anything else in the store
  directory: the bundle is an allow-list, never a glob;
- the HandedOff line: it is written only to the exporting host's log.

Export never moves the user's index, HEAD or branch: the tree is built from
a temporary index (`GIT_INDEX_FILE`) and only the two refs are written.

## The furrow carrier

`[handoff] carrier = "furrow"` with `furrow_store = "s3://bucket/prefix"` (see
docs/CONFIG.md for every setting and variable). Export runs, in the working
tree:

1. writes the session bundle (the same allow-listed tar) to
   `.modron/handoff/<id>.handoff.tar`;
2. `furrow remote add <store> --name modron-<id> --key <recovery key>`;
3. `furrow snap -m ...`, then `furrow --json sync --push --takeover` (the
   takeover is safe here because ownership is the log prefix rule below, not
   furrow's writer lease);
4. removes the bundle file (and `.modron/handoff` when empty) whatever the
   outcome, then appends `HandedOff` as on the git carrier.

The bundle therefore travels inside the furrow snapshot and never stays in the
user's tree. The snapshot holds the whole working tree, ignored files, `.git`
and the bundle. The printed carrier ref is
`furrow:<store>/modron-<id>@<snapshot>`.

Import runs `furrow clone <url> <scratch> --no-watch` with the key in
`FURROW_RECOVERY_KEY`, reads the bundle from the clone, and then applies the
same rules as git: the log decision and the dirty-tree check run before any
file changes; a file the handoff has that is ignored or untracked here needs
`--force`. The clone's `.git`, furrow state and bundle are not copied; the
other files are copied over the checkout, and tracked files the handoff no
longer has are removed. The checkout's own HEAD and branch are not changed.
Import needs a git checkout in the working directory.

Furrow is found on PATH, or via `MODRON_FURROW_BIN`. A missing binary, an
unset `furrow_store` or `FURROW_RECOVERY_KEY`, or a non-zero furrow exit is an
error that names the step; furrow's stderr is passed through with the secret
values removed.

## Exit hand-off (serve)

`modron serve --remote-control <bridge> --handoff-on-exit` (or
`MODRON_HANDOFF_ON_EXIT=1`) exports the session that remote control serves when
serve stops on ctrl+c or SIGTERM. Serve first interrupts that session's run,
then runs the configured export on its own thread and waits at most 30 seconds.
A failure or a timeout is printed and serve exits anyway. The git export pushes
to `origin`, so the exiting host needs push access to it.

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
- Furrow 0.3.7 (`clone`) imports only `s3://` and `ssh://` stores.
- Two writers are not guarded beyond the log prefix rule. The furrow export
  takes over furrow's writer lease, so exporting while another host still
  runs the session is not prevented by furrow.
- Provider credentials do not move; each host signs in on its own.
