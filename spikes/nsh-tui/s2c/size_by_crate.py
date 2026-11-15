"""Aggregate `nm --size-sort -S -C --defined-only` output by crate.

Usage: python3 size_by_crate.py <nm-output-file> [top_n]
Crate = first path segment of the demangled Rust symbol (leading '<' and
'_$LT$' stripped). Non-path symbols are bucketed as 'C/other'.
"""
import re
import sys
from collections import defaultdict

LINE = re.compile(r"^([0-9a-fA-F]+) ([0-9a-fA-F]+) (\S) (.*)$")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def crate_of(name):
    s = name.strip()
    if s.startswith("<"):
        s = s[1:]
    s = s.replace("_$LT$", "").replace("_$GT$", "")
    m = IDENT.match(s)
    if m and "::" in s[: m.end() + 2]:
        return m.group(0)
    if "::" in s:
        return s.split("::")[0]
    return "C/other"


def main():
    path = sys.argv[1]
    top_n = int(sys.argv[2]) if len(sys.argv) > 2 else 20
    total = defaultdict(int)
    count = defaultdict(int)
    grand = 0
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            m = LINE.match(line.rstrip("\n"))
            if not m:
                continue
            size = int(m.group(2), 16)
            c = crate_of(m.group(4))
            total[c] += size
            count[c] += 1
            grand += size
    print(f"symbols-with-size total_bytes={grand}")
    print(f"{'crate':<32}{'bytes':>12}{'share':>8}{'syms':>8}")
    for c, b in sorted(total.items(), key=lambda kv: -kv[1])[:top_n]:
        print(f"{c:<32}{b:>12}{100.0 * b / grand:>7.1f}%{count[c]:>8}")


if __name__ == "__main__":
    main()
