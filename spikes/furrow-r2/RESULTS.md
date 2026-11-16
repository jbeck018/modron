# Packet S3: furrow snapshot/restore over Cloudflare R2

Date: 2026-11-16. Bucket: modron-furrow-spike (one bucket, one namespace,
deleted afterwards; bucket left empty). Driver: spikes/furrow-r2/spike.sh
(reads credentials from the environment only).

## Answers

1. furrow 0.3.7 (Agent-Field/furrow main at dac1280), Rust, Apache-2.0. A
   snapshot is the whole working tree minus `.furrowpolicy` exclusions:
   ignored files, `.git`, symlinks and modes are all captured (verified).
   Backends: encrypted local directory, ssh://, and s3:// (S3-compatible).
2. Yes, against R2 with no code change. Full round trip (snapshot, clone into
   an empty directory with a fresh store, diff) matched byte for byte and mode
   for mode. A one-file change uploaded 4 objects (3.9 KB) and reused 158.
3. Not needed. Nothing failed. Conditional writes (If-None-Match create, If-Match
   head CAS) were accepted by R2. The only R2-specific setting is path-style
   addressing, which furrow turns on automatically with a custom endpoint.
4. Modron would call the `furrow` CLI from the container image (`--json` is the
   documented agent API), with secrets from Worker secrets. The fallback needs
   extra care for ignored and untracked files. Concurrency and takeover are not
   tested; the code suggests conditional head writes plus an explicit
   `sync --push --takeover`.
5. Use furrow over R2 as the working-state transport, with no patch, but ship
   the git-branch fallback first for committed work. Treat furrow as the next
   increment, gated on a two-writer test and on upload throughput (see below).

## 1. What furrow is

- Source: furrow-core (library `furrow`), furrow-cli (binary `furrow`),
  furrow-daemon. Pre-1.0 (v0.3.7), Apache-2.0, no build.rs. Built with
  `RUSTUP_TOOLCHAIN=stable cargo build --release --locked -p furrow-cli`
  (cargo 1.99.1; the pinned 1.97.1 toolchain was not installed; 2m48s).
- Snapshot content (README and repository.rs): the working tree including
  ignored files and dependencies, minus `.furrowpolicy` rules (`exclude <dir>`,
  `gitignore`). Content is chunked, content-addressed (BLAKE3) and encrypted
  (XChaCha20-Poly1305) before it reaches a remote. Restore covers symlinks,
  permissions, xattrs, hard links, and SQLite (logical image). Not checked:
  xattrs, hard links, SQLite.
- Remote backends (furrow-core/src/remote.rs): local encrypted directory, ssh://,
  s3://bucket/prefix.
- S3 configuration (furrow-core/src/s3_remote.rs): region from AWS_REGION
  (or AWS_DEFAULT_REGION); credentials from AWS_ACCESS_KEY_ID and
  AWS_SECRET_ACCESS_KEY via rusty-s3; custom endpoint from FURROW_S3_ENDPOINT
  (https only, http needs FURROW_S3_ALLOW_HTTP=1); FURROW_S3_PATH_STYLE=true|false,
  which defaults to path-style when a custom endpoint is set. Requests use
  presigned URLs (5 min TTL) and a 60 s timeout. Nothing in the code is
  Cloudflare-specific; a custom endpoint works.
- Object layout: `<prefix>/<namespace>/objects/...` plus a per-ref HEAD pointer
  updated with a compare-and-swap (If-Match on its ETag).

## 2. Run against R2

Test tree (/tmp/claude-0/furrow-work/src-repo, not in the repo): one commit
(README.md, run.sh mode 755, src/main.rs, symlink link-to-readme, .gitignore
with `*.log` and `build/`); uncommitted README.md edit; untracked notes.txt
and assets/; ignored ignored.log and build/out.bin; assets/blob.bin 20 MiB random.

Commands (furrow with HOME in a throwaway dir, FURROW_S3_ENDPOINT from the
environment, FURROW_S3_PATH_STYLE=true):

    furrow watch --no-daemon
    furrow remote add s3://modron-furrow-spike/<ns> --name spike
    furrow sync --push --timings
    HOME=<fresh> FURROW_RECOVERY_KEY=<key> furrow clone s3://modron-furrow-spike/<ns>/spike <dir> --no-watch

Results (trimmed):

    snapshot 1: 162 objects uploaded (21,048,084 bytes payload); bucket 21,048,552 bytes
                202.6 s total (stream 199.6 s; ~105 KB/s); sequential PUTs
    restore 1:  fresh store, empty dir: 129 objects fetched (21,016,780 bytes), 48.4 s
    compare restore1 vs copy of source at snapshot 1:
                metadata (%M mode, %s size, symlink target) identical
                sha256 of all regular files identical (incl. 20 MiB binary,
                ignored.log, build/out.bin, .git)
                git log identical; git status: " M README.md", "?? notes.txt",
                "?? assets/", "?? .furrow/"
    snapshot 2 (README.md changed only): 4 objects uploaded (3,895 bytes), 158 reused;
                bucket +4,019 bytes; 66.1 s (stream 61.7 s, not proportional; not investigated)
    restore 2:  fresh store: 129 objects fetched again, 33.4 s; identical to source
    cleanup:    174 objects deleted; namespace listing empty; probe object removed

Raw context: a 1 MiB PUT, GET and HEAD to the same bucket from this sandbox took
0.70 s, 0.69 s and 0.21 s, so furrow's ~105 KB/s is its sequential upload, not
the link.

What survived: content, modes (including the exec bit), symlinks, ignored
files, the .git directory, uncommitted and untracked work, binary bytes.
What did not: nothing was lost. Extra artifact: `.furrow/` appears in the
restored tree (identity pointers) and must be excluded by Modron.
Incremental: yes, on the upload side (a one-file change uploaded 4 objects).
Restores always fetch all objects into a fresh store.

## 3. Failures and gaps

- No request failed. R2 accepted If-None-Match: * on object creation and
  If-Match on the HEAD pointer (the second snapshot's CAS succeeded).
- Not tested: two writers or the takeover path; interrupted upload; xattrs,
  hard links, SQLite; a running process during snapshot; `furrow serve`/MCP;
  large trees (the test tree had 39 files). Throughput: about 105 KB/s, so a
  1 GB dependency directory would take roughly 3 hours to upload. Use the
  `.furrowpolicy` exclusions.
- The snapshot-2 time (66 s for 4 objects) is unexplained.

## 4. What Modron would need

- Invocation: the `furrow` binary in the container image, called with `--json`
  (README: the CLI is the agent API). Library use (furrow-core) would tie Modron to
  a pre-1.0 API. Snapshot on sleep: `furrow sync --push`. Restore on a new host:
  `furrow clone` into an empty directory, or `sync --pull` for an existing one.
- Credentials: Worker secrets injected as env into the container:
  AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION=auto,
  FURROW_S3_ENDPOINT, FURROW_S3_PATH_STYLE=true, plus a per-workspace
  FURROW_RECOVERY_KEY. Losing the recovery key makes the remote unreadable;
  the remote holds only ciphertext.
- Concurrency: the code has a per-ref HEAD with compare-and-swap and an explicit
  takeover (`sync --push --takeover`). The README says cross-machine divergence
  is reported, not merged. The PLAN rule (one owner at a time) is the right
  model; enforce it in Modron.
- Git-branch fallback: commits and pushes only tracked work. Ignored, untracked
  and dirty `.env`/DB state are lost unless committed (committing secrets is
  wrong). Pros: no new dependency, auditable, fast, uses GitHub access that
  sessions already have. Cons: needs push access to the session's repositories,
  and nothing else crosses the handoff.

## 5. Recommendation

Ship the git-branch fallback first, since it covers committed work with no new
dependency. Then add furrow as the working-state transport over R2 (no patch
needed), gated on: a two-writer and takeover test, an interrupted-upload test,
a check of `.env` and SQLite handling, and an upload throughput fix (parallel
PUTs) or a tight `.furrowpolicy` before anything large.

## Reproduce

    set -a; . <env file with AWS_* , AWS_ENDPOINT_URL, R2_BUCKET>; set +a
    bash spikes/furrow-r2/spike.sh <furrow binary> <empty work dir> <git repo to snapshot>
