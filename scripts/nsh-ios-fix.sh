#!/bin/sh
# Fix the broken nsh-platform-ios 0.4.0 crates.io package so
# `nsh package --target ios` / `--target ios-simulator` can build.
#
# Defect: the published crate's src/lib.rs declares
#   #[path = "../../nsh-platform-macos/src/speech.rs"] mod speech;
# a path outside the published package. It only resolved inside the nsh
# monorepo, so registry builds fail with "couldn't find file ...".
#
# Fix: cargo resolves that path as
#   <registry-src>/nsh-platform-ios-0.4.0/src/../../nsh-platform-macos/src/speech.rs
# i.e. <registry-src>/nsh-platform-macos/src/speech.rs — one symlink from
# the unversioned name to the extracted nsh-platform-macos-0.4.0 package
# makes it resolve into the genuine macOS crate source (exactly what
# upstream's in-repo include intended). Cargo does not re-verify extracted
# registry packages, so one link per machine fixes every future build.
#
# NOTE: a [patch.crates-io] in mobile/app/Cargo.toml does NOT work — the
# per-target runner manifests under mobile/app/.nsh/rust/ are regenerated
# from templates on every nsh run and resolve independently. Verified
# 2026-11-09; see mobile/SPIKE.md. Upstream main has since moved speech
# into a shared nsh-platform-apple crate; drop this script once an nsh
# release publishes the fixed iOS platform crate.
#
# Idempotent. Run once per machine (and after wiping the cargo registry).
set -eu

reg="${CARGO_HOME:-$HOME/.cargo}/registry/src"
src_dir=$(ls -d "$reg"/*/ 2>/dev/null | head -1 || true)
if [ -z "$src_dir" ]; then
    echo "error: no cargo registry src dir under $reg" >&2
    exit 1
fi

if [ ! -d "${src_dir}nsh-platform-ios-0.4.0" ]; then
    echo "nsh-platform-ios-0.4.0 not extracted yet - run the iOS packaging" >&2
    echo "once (it will fail), then rerun this script." >&2
    exit 1
fi
if [ ! -d "${src_dir}nsh-platform-macos-0.4.0" ]; then
    echo "error: ${src_dir}nsh-platform-macos-0.4.0 missing" >&2
    exit 1
fi

ln -sfn nsh-platform-macos-0.4.0 "${src_dir}nsh-platform-macos"
echo "linked ${src_dir}nsh-platform-macos -> nsh-platform-macos-0.4.0"
echo "iOS packaging should now build: nsh package app --target ios-simulator"
