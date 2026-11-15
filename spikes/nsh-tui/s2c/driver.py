"""S2c driver: builds spike configurations, strips copies, runs headless modes.

Writes s2c/results.tsv (one row per configuration) and s2c/logs/<name>.txt.
Phase 1 = feature configs C0..C4 on the z+fat+cgu1+abort profile.
Phase 2 = profile levers on the smallest passing feature config.
Phase 3 = ref-ratatui hello world, same profile as the base config.
"""
import glob, os, re, shutil, subprocess, sys, time

SPIKE = "/home/user/modron/spikes/nsh-tui"
S2C = f"{SPIKE}/s2c"
ENV = dict(os.environ, CARGO_TARGET_DIR=f"{SPIKE}/target", CARGO_INCREMENTAL="0")
REG = glob.glob(os.path.expanduser("~/.cargo/registry/src/*/nsh-platform-tui-0.5.1"))[0]
VENDOR = f"{SPIKE}/vendor/nsh-platform-tui"
RESULTS = f"{S2C}/results.tsv"
MIN_FREE_GB = 3.0

def log(msg):
    with open(f"{S2C}/driver.log", "a") as fh:
        fh.write(time.strftime("%H:%M:%S ") + msg + "\n")

def free_gb():
    st = os.statvfs("/")
    return st.f_bavail * st.f_frsize / 1e9

def profile_text(p):
    lines = ["", "[profile.release]", f'opt-level = "{p["opt"]}"', 'lto = "fat"',
             "codegen-units = 1", f'panic = "{p["panic"]}"', "strip = false"]
    if p.get("deps_z"):
        lines += ["", '[profile.release.package."*"]', 'opt-level = "z"']
    return "\n".join(lines) + "\n"

def write_vendor(cfg):
    """Copy pristine nsh-platform-tui and apply the feature edits."""
    if os.path.exists(VENDOR):
        shutil.rmtree(VENDOR)
    shutil.copytree(REG, VENDOR)
    path = f"{VENDOR}/Cargo.toml"
    text = open(path).read()
    def set_features(text, header, new):
        # replace the `features = [...]` array inside the given [dependencies.X] table
        i = text.index(header)
        j = text.find("\n[", i + len(header))
        j = len(text) if j < 0 else j
        block = text[i:j]
        if new is None:
            block2 = re.sub(r"features = \[[^\]]*\]\n", "", block)
        else:
            items = ", ".join(f'"{f}"' for f in new)
            block2 = re.sub(r"features = \[[^\]]*\]", f"features = [{items}]", block, flags=re.S)
        return text[:i] + block2 + text[j:]
    if cfg.get("nbase") is not None:
        text = set_features(text, "[dependencies.nsh-base]", cfg["nbase"])
    if cfg.get("engine_net") is False:
        text = set_features(text, "[dependencies.nsh-engine]", None)
    open(path, "w").write(text)
    if cfg.get("engine_net") is False:
        # the only use of the `net` feature in nsh-platform-tui: the network handle
        app = f"{VENDOR}/src/app.rs"
        src = open(app).read()
        old = "config.network = Some(nsh_engine::net::network());"
        assert src.count(old) == 1, "net call site not found"
        open(app, "w").write(src.replace(old, "config.network = None;"))

def write_manifest(cfg, profile):
    ratatui = ('ratatui = { version = "=0.30.2", default-features = false, features = ["std"] }'
               if cfg.get("ratatui_nodef") else 'ratatui = "=0.30.2"')
    patch = ('\n[patch.crates-io]\nnsh-platform-tui = { path = "vendor/nsh-platform-tui" }\n'
             if cfg.get("vendor") else "")
    text = f'''[package]
name = "nsh-tui-spike"
version = "0.1.0"
edition = "2021"
publish = false

# Own workspace root: independent of the modron root workspace.
[workspace]

[dependencies]
nsh-core = "=0.5.1"
nsh-platform-tui = {{ version = "=0.5.1", default-features = false }}
{ratatui}
tokio = {{ version = "1", default-features = false, features = ["sync"] }}
{patch}{profile_text(profile)}'''
    open(f"{SPIKE}/Cargo.toml", "w").write(text)

def build(name):
    t = time.time()
    p = subprocess.run(["cargo", "build", "--release"], cwd=SPIKE, env=ENV,
                       capture_output=True, text=True)
    secs = time.time() - t
    if p.returncode != 0:
        tail = "\n".join((p.stderr or "").splitlines()[-40:])
        open(f"{S2C}/logs/{name}.build_error.txt", "w").write(p.stderr)
        log(f"{name} BUILD FAILED after {secs:.0f}s: {tail[-600:]}")
        return None, secs
    return f"{SPIKE}/target/release/nsh-tui-spike", secs

def measure(binary, name):
    out = f"{S2C}/logs/{name}.stripped"
    subprocess.run(["strip", "-o", out, binary], check=True)
    size = os.path.getsize(out)
    pk = subprocess.run("cargo tree -e normal --prefix none | sort -u | wc -l", shell=True,
                        cwd=SPIKE, env=ENV, capture_output=True, text=True).stdout.strip()
    return size, pk

def headless(binary, name):
    modes = [["startup"], ["stream", "0"], ["scroll"], ["input"], ["external", "0", "0"], ["focus"]]
    ok = True
    lines = []
    for m in modes:
        try:
            env = dict(os.environ, NSH_SPIKE_FOCUS="submit")  # default "blur" fails by design (RESULTS C)
            p = subprocess.run([binary] + m, capture_output=True, text=True, timeout=300, env=env)
            out = p.stdout + p.stderr
            bad = p.returncode != 0 or re.search(r"ERROR|FAIL|panick|timed out", out)
            ok = ok and not bad
            lines.append(f"== {' '.join(m)} rc={p.returncode} {'FAIL' if bad else 'ok'}\n{out[-1500:]}")
        except subprocess.TimeoutExpired:
            ok = False
            lines.append(f"== {' '.join(m)} TIMEOUT")
    open(f"{S2C}/logs/{name}.headless.txt", "w").write("\n".join(lines))
    return ok

def run_cfg(name, cfg, profile):
    if free_gb() < MIN_FREE_GB:
        log(f"STOP disk free {free_gb():.1f} GB < {MIN_FREE_GB}")
        sys.exit(1)
    if cfg.get("vendor"):
        write_vendor(cfg)
    write_manifest(cfg, profile)
    binary, secs = build(name)
    if binary is None:
        row = [name, f"{secs:.0f}", "BUILD_FAIL", "", "", ""]
    else:
        size, pk = measure(binary, name)
        hl = headless(binary, name)
        row = [name, f"{secs:.0f}", str(size), pk, "pass" if hl else "FAIL", ""]
    with open(RESULTS, "a") as fh:
        fh.write("\t".join(row) + "\n")
    log(f"{name}: {row}")
    return row

BASE_PROFILE = {"opt": "z", "panic": "abort"}
FEATURE_CFGS = [
    ("C0_base", {}),
    ("C1_ratatui_nodef", {"ratatui_nodef": True}),
    ("C2_plus_no_highlight", {"ratatui_nodef": True, "vendor": True, "nbase": ["editor", "emoji-picker", "plot", "rich-editor", "term"]}),
    ("C3_plus_no_net", {"ratatui_nodef": True, "vendor": True, "engine_net": False,
                        "nbase": ["editor", "emoji-picker", "plot", "rich-editor", "term"]}),
    ("C4_plus_no_nbase_extras", {"ratatui_nodef": True, "vendor": True, "engine_net": False, "nbase": []}),
]

def main():
    resume = sys.argv[1:] == ["resume"]
    done = set()
    if resume and os.path.exists(RESULTS):
        done = {l.split("\t")[0] for l in open(RESULTS).read().splitlines() if "\tpass" in l}
    else:
        if os.path.exists(RESULTS):
            os.remove(RESULTS)
        open(f"{S2C}/driver.log", "w").close()
    for name, cfg in FEATURE_CFGS:
        if name not in done:
            run_cfg(name, cfg, BASE_PROFILE)
    rows = [l.split("\t") for l in open(RESULTS).read().splitlines()]
    rows = [r for r in rows if r[0] in dict(FEATURE_CFGS)]
    passing = [r for r in rows if r[4] == "pass"]
    best_name = min(passing, key=lambda r: int(r[2]))[0]
    best_cfg = dict(FEATURE_CFGS)[best_name]
    log(f"best feature config: {best_name}")
    run_cfg(f"P1_{best_name}_opt_s_abort", best_cfg, {"opt": "s", "panic": "abort"})
    run_cfg(f"P2_{best_name}_opt_z_unwind", best_cfg, {"opt": "z", "panic": "unwind"})
    run_cfg(f"P3_{best_name}_app_s_deps_z", best_cfg, {"opt": "s", "panic": "abort", "deps_z": True})
    log("driver phase 2 done")

if __name__ == "__main__":
    main()
