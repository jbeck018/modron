# Oculus theme tokens for the unified client

`oculus.json` is the NSH theme file for the unified client app. It is derived
from the desktop brand theme in
`crates/modron-desktop/assets/oculus-theme.json` (gpui-kit ThemeSet, "Oculus
Dark" and "Oculus Light") and the type scale in
`crates/modron-desktop/src/brand.rs`. Every colour is a source hex, copied
unchanged in case. No oklch conversion and no new colours.

## Schema

The file uses shadcn/ui's `cssVars` shape, as the NSH docs define it:
`{ "cssVars": { "theme": {...}, "light": {...}, "dark": {...} } }`.
Source: https://nsh.dev/docs/rust.md#theme (also linked from
https://nsh.dev/docs/project-structure.md and https://nsh.dev/docs/tailwind.md#theme-tokens).
`light` holds the light values, `dark` the dark values, and `theme` holds
the radius and fonts for both.

To use it, `nsh.json` (the file in `mobile/app/` is not changed here) needs:

```json
"theme": "oculus.json"
```

The path is relative to `nsh.json`, so the file must sit beside it or the
path must be adjusted.

## Token mapping

| token | dark | light | source role in oculus-theme.json |
|---|---|---|---|
| background | #000000 | #FFFFFF | background |
| foreground | #E2E2E2 | #000000 | foreground |
| card | #1C1C1C | #F7F7F7 | popover.background (CHOICE: no card role) |
| card-foreground | #F0F0F0 | #1F1F1F | popover.foreground (CHOICE) |
| popover | #1C1C1C | #F7F7F7 | popover.background |
| popover-foreground | #F0F0F0 | #1F1F1F | popover.foreground |
| primary | #D9A520 | #D9A520 | primary.background |
| primary-foreground | #000000 | #1A1A1A | primary.foreground |
| secondary | #242424 | #F0F0F0 | secondary.background |
| secondary-foreground | #E2E2E2 | #1A1A1A | secondary.foreground |
| muted | #333333 | #E2E2E2 | muted.background |
| muted-foreground | #ADADAD | #5F5F5F | muted.foreground |
| accent | #3A2E17 | #FDF5E3 | accent.background |
| accent-foreground | #E9C34A | #7A5A08 | accent.foreground |
| destructive | #E5484D | #C0201C | danger.background |
| border | #2A2A2A | #D2D6DC | border |
| input | #2A2A2A | #D2D6DC | input.border |
| ring | #D9A520 | #D9A520 | ring |
| chart-1 | #2EA043 | #1A7F43 | chart.bullish (CHOICE: no chart palette) |
| chart-2 | #F85149 | #C0201C | chart.bearish (CHOICE) |
| chart-3 | #D9A520 | #D9A520 | primary.background (CHOICE) |
| chart-4 | #58A6FF | #0A5BC4 | info.background (CHOICE) |
| chart-5 | #E0912A | #8A5A0B | warning.background (CHOICE) |
| sidebar | #000000 | #FFFFFF | list.background (CHOICE: no sidebar role) |
| sidebar-foreground | #E2E2E2 | #000000 | foreground (CHOICE) |
| sidebar-primary | #D9A520 | #D9A520 | primary.background (CHOICE) |
| sidebar-primary-foreground | #000000 | #1A1A1A | primary.foreground (CHOICE) |
| sidebar-accent | #242424 | #F0F0F0 | list.hover.background (CHOICE) |
| sidebar-accent-foreground | #E2E2E2 | #000000 | foreground (CHOICE) |
| sidebar-border | #2A2A2A | #D2D6DC | border (CHOICE) |
| sidebar-ring | #D9A520 | #D9A520 | ring (CHOICE) |
| success | #3FB950 | #1A7F43 | success.background |
| warning | #E0912A | #8A5A0B | warning.background |
| info | #58A6FF | #0A5BC4 | info.background |
| link | #E9C34A | #8A6510 | link |
| tool | #58A6FF | #0A5BC4 | info.background (CHOICE: no tool role) |
| dim | #ADADAD | #5F5F5F | muted.foreground (CHOICE: no dim role) |

Theme section (both appearances):

| token | value | source |
|---|---|---|
| radius | 0.25rem | `radius: 4` (points) at 16 px = 1rem |
| font-sans | DM Sans, sans-serif | `font.family: "DM Sans"` |
| font-mono | Menlo, Consolas, monospace | CHOICE: the source names no mono family |

## Type scale

From `crates/modron-desktop/src/brand.rs`, `text` module. The theme file
carries no font sizes; clients take these constants.

| name | size (pt) | use |
|---|---|---|
| XS | 11 | timestamps, badges, tiny labels |
| SM | 12 | secondary and meta text |
| BASE | 13 | body: transcript, session rows, inputs |
| LG | 14 | titles, section headers |
| XL | 16 | panel titles |
| DISPLAY | 28 | the greeting display line |

## Terminal mapping

The six terminal slots in `crates/modron-tui/src/theme.rs`, `oculus` theme:

| slot | token | dark value | TUI oculus hex |
|---|---|---|---|
| accent | primary | #D9A520 | d9a520 (same) |
| dim | muted-foreground | #ADADAD | adadad (same) |
| tool | tool (= info.background) | #58A6FF | 58a6ff (same) |
| warning | warning | #E0912A | e0912a (same) |
| error | destructive | #E5484D | e5484d (same) |
| success | success | #3FB950 | 3fb950 (same) |

Hex differences between the TUI `oculus` slots and the JSON: none, when
compared case-insensitively. The TUI `oculus` theme has no light variant,
so the light values have no terminal counterpart.

## Choices that need a human decision

1. `card`, `card-foreground`: popover roles used (no card role in source).
2. `sidebar*` (8 tokens): mapped to list and primary roles (no sidebar role).
3. `chart-1` to `chart-5`: source has no chart palette. Used bullish, bearish,
   primary, info and warning. Pick a real categorical palette if needed.
4. `tool`: the source has no `tool` role; the task named one, so it is set to
   info.background, which matches the TUI oculus tool hex in dark.
5. `dim`: muted.foreground stands in (no dim role in source).
6. `font-mono`: source names no monospace face. Menlo, Consolas and the generic
   monospace family are a placeholder.
7. `radius.lg` (12) is not mapped; only `radius` (4) is in the theme section.
8. `mono_font.size` (12.5) is not in the type scale in brand.rs; the scale
   constants were used as the authority.
9. Light `card` and `popover` use #F7F7F7, the light popover value.
10. The dark `card` and `popover` values are #1C1C1C with #F0F0F0 text,
    which come from the dark popover roles.

## Check

`python3 -I check.py` (stdlib only) loads `oculus.json`, checks that all 37
colour tokens exist in both `light` and `dark`, that the `theme` keys exist,
and that every colour is `#rrggbb` or `#rrggbbaa`.
