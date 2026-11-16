#!/usr/bin/env python3
"""Check app/theme/oculus.json: required tokens in light and dark, hex colours."""
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PATH = os.path.join(HERE, "oculus.json")

COLOUR_TOKENS = [
    "background", "foreground", "card", "card-foreground", "popover",
    "popover-foreground", "primary", "primary-foreground", "secondary",
    "secondary-foreground", "muted", "muted-foreground", "accent",
    "accent-foreground", "destructive", "border", "input", "ring",
    "chart-1", "chart-2", "chart-3", "chart-4", "chart-5",
    "sidebar", "sidebar-foreground", "sidebar-primary",
    "sidebar-primary-foreground", "sidebar-accent",
    "sidebar-accent-foreground", "sidebar-border", "sidebar-ring",
    "success", "warning", "info", "link", "tool", "dim",
]
THEME_KEYS = ["radius", "font-sans", "font-mono"]
HEX = re.compile(r"^#(?:[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$")


def fail(msg):
    print("FAIL: " + msg)
    sys.exit(1)


def main():
    with open(PATH, encoding="utf-8") as fh:
        doc = json.load(fh)
    css = doc.get("cssVars")
    if not isinstance(css, dict):
        fail("missing cssVars object")
    for section in ("theme", "light", "dark"):
        if not isinstance(css.get(section), dict):
            fail("missing section " + section)
    for key in THEME_KEYS:
        if key not in css["theme"]:
            fail("theme section lacks " + key)
    for mode in ("light", "dark"):
        for token in COLOUR_TOKENS:
            if token not in css[mode]:
                fail("%s lacks token %s" % (mode, token))
            value = css[mode][token]
            if not isinstance(value, str) or not HEX.match(value):
                fail("%s %s is not #rrggbb or #rrggbbaa: %r" % (mode, token, value))
    print("ok %d tokens" % len(COLOUR_TOKENS))


if __name__ == "__main__":
    main()
