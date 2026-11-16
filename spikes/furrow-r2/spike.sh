#!/usr/bin/env bash
# Furrow-over-R2 spike driver (furrow 0.3.7). Credentials come ONLY from the environment:
#   AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_REGION AWS_ENDPOINT_URL R2_BUCKET
# Usage: spike.sh <furrow-binary> <work-dir> <src-repo>
# Writes only under <work-dir> and under a fresh namespace in R2_BUCKET; deletes that namespace on exit.
set -uo pipefail
BIN=$1; WORK=$2; SRC=$3
: "${AWS_ACCESS_KEY_ID:?}" "${AWS_SECRET_ACCESS_KEY:?}" "${AWS_ENDPOINT_URL:?}" "${R2_BUCKET:?}"
export AWS_REGION="${AWS_REGION:-auto}"
export HOME="$WORK/home"                      # throwaway HOME for furrow state
export FURROW_S3_ENDPOINT="$AWS_ENDPOINT_URL" # furrow's own names for the endpoint and path style
export FURROW_S3_PATH_STYLE=true
mkdir -p "$HOME"
NS="spike-$(date +%s)"
S3URL="s3://$R2_BUCKET/$NS"
AUTH=(--aws-sigv4 "aws:amz:auto:s3" --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY")
log() { printf '\n== %s\n' "$*"; }
# "size key" lines for this namespace only (ListObjectsV2 with prefix)
s3_list() {
  curl -sS "${AUTH[@]}" "$AWS_ENDPOINT_URL/$R2_BUCKET?list-type=2&prefix=$NS/" -o "$WORK/list.xml"
  python3 -I -c 'import sys,xml.etree.ElementTree as E
ns={"s":"http://s3.amazonaws.com/doc/2006-03-01/"}
r=E.parse(sys.argv[1]).getroot()
for c in r.iter():
    if c.tag.endswith("Contents"):
        k=[x.text for x in c if x.tag.endswith("Key")][0]
        s=[x.text for x in c if x.tag.endswith("Size")][0]
        print(s, k)' "$WORK/list.xml"
}
s3_total() { s3_list | awk '{n++; s+=$1} END{printf "objects=%d bytes=%d\n", n, s}'; }
cleanup() {
  s3_list | awk '{print $2}' | while read -r k; do
    curl -sS -X DELETE "${AUTH[@]}" "$AWS_ENDPOINT_URL/$R2_BUCKET/$k" -o /dev/null -w 'delete %{http_code}\n'
  done
  echo "after cleanup: $(s3_total)"
}
trap 'log "cleanup"; cleanup' EXIT
now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2fs", b-a}'; }

cd "$SRC" || exit 1
log "watch (first local capture, no daemon)"
"$BIN" watch --no-daemon 2>&1 | tail -5

log "remote add (pair) ${S3URL}"
"$BIN" remote add "$S3URL" --name spike > "$WORK/remote-add.txt" 2>&1; echo "exit=$?"
# print the summary with 64-hex recovery keys redacted
sed -E 's/[0-9a-f]{64}/<KEY-REDACTED>/g' "$WORK/remote-add.txt" | head -20
CLONE_URL=$(grep -o 's3://[^ ]*' "$WORK/remote-add.txt" | tail -1)
KEY=$(grep -o -E '[0-9a-f]{64}' "$WORK/remote-add.txt" | head -1)
[ -n "$KEY" ] || { echo "no recovery key found"; exit 1; }
echo "clone url: $CLONE_URL"

cp -a "$SRC" "$WORK/state1"   # expected tree at snapshot 1
log "snapshot 1: sync --push"
b0=$(s3_total); t0=$(now)
"$BIN" sync --push --timings 2>&1 | tail -15; echo "exit=${PIPESTATUS[0]}"
t1=$(now); b1=$(s3_total)
echo "elapsed $(elapsed "$t0" "$t1"); before: $b0; after: $b1"

# Each restore gets its own HOME (its own empty furrow store), so every object is fetched from R2.
compare() { # compare <restored-dir> <expected-state-dir>: modes, symlinks, sizes, contents, git status
  local R=$1 E=$2
  tree() { (cd "$1" && find . -path ./.furrow -prune -o -printf '%M %s %p -> %l\n' | sort); }
  hashes() { (cd "$1" && find . -path ./.furrow -prune -o -type f -print0 | sort -z | xargs -0 sha256sum); }
  echo "-- metadata diff (mode size path -> link), expected vs restored:"
  diff <(tree "$E") <(tree "$R") && echo "   identical"
  echo "-- content diff (sha256 over regular files):"
  diff <(hashes "$E") <(hashes "$R") && echo "   identical"
  echo "-- git status --short in restored tree:"; git -C "$R" status --short | sed 's/^/   /'
  echo "-- git log:"; git -C "$R" log --oneline | sed 's/^/   /'
}

log "restore 1: clone snapshot 1 into empty dir with fresh HOME"
mkdir -p "$WORK/home-r1"
t2=$(now)
HOME="$WORK/home-r1" FURROW_RECOVERY_KEY="$KEY" "$BIN" clone "$CLONE_URL" "$WORK/restore1" --no-watch 2>&1 | tail -8; echo "exit=${PIPESTATUS[0]}"
echo "clone elapsed $(elapsed "$t2" "$(now)")"
compare "$WORK/restore1" "$WORK/state1"

log "incremental: change one small file, snapshot 2"
printf 'hello v3 changed\n' > "$SRC/README.md"
b2=$(s3_total); t4=$(now)
"$BIN" sync --push --timings 2>&1 | tail -15; echo "exit=${PIPESTATUS[0]}"
t5=$(now); b3=$(s3_total)
echo "elapsed $(elapsed "$t4" "$t5"); before: $b2; after: $b3"

log "restore 2: clone snapshot 2 into second empty dir with fresh HOME"
mkdir -p "$WORK/home-r2"
t6=$(now)
HOME="$WORK/home-r2" FURROW_RECOVERY_KEY="$KEY" "$BIN" clone "$CLONE_URL" "$WORK/restore2" --no-watch 2>&1 | tail -4; echo "exit=${PIPESTATUS[0]}"
echo "clone2 elapsed $(elapsed "$t6" "$(now)")"
compare "$WORK/restore2" "$SRC"
