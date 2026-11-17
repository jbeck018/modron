# Web design standard (app/)

The look of the web target. The terminal target is not restyled: every
style applies only where `chat::WEB_CONTROLS` is true, and `ui::screen`
returns the body unchanged on the terminal.

## The rule for every screen

Build controls from the `ui.rs` components, in shadcn style, on `nsh_base`.
Do not hand-roll a button, an input, a card or a row in a screen. Each
component is a thin, open wrapper: behaviour, state, focus and accessibility
come from an `nsh_base` component where one exists; the look comes from this
app's tokens and sizes. Where `nsh_base` has no component, the component is
composed from NSH primitives in the same style, and is listed below.

## Tokens

- Colours come only from the Oculus theme (`theme/oculus.json`), read with
  `ui::tok("name")` or `chat::token_ink("name")`. No literal colour in a
  screen or in `ui.rs`. Light and dark follow the system appearance.
- Token names are shadcn's: background, foreground, card, card-foreground,
  popover, primary, primary-foreground, secondary, secondary-foreground,
  muted, muted-foreground, accent, destructive, destructive-foreground (added
  to the theme for this), border, input, ring. The app also reads the Oculus
  extras success, warning, link and tool.
- Radius: the `radius` token, read with `ui::radius()`. The Oculus file names
  none, so NSH's default (12 pt) applies.
- Fonts: the theme names no font family, so text uses the system stack. DM
  Sans is not shipped (it would need `nsh.json` font wiring).

## Type, spacing, layout

- Type: caption 13 pt muted (`ui::caption`); body 16 pt, 15 pt from 640 pt
  wide (`body_size`; inputs are 16 pt always, so iOS does not zoom); title 17
  pt semibold; large title 24 pt semibold (the sign-in heading); wordmark 20
  pt. Line height is NSH's default.
- Spacing scale: 4, 8, 12, 16, 24, 32 points only.
- Page gutter 16 pt on phones, 24 pt from 640 pt of window width
  (`ui::gutter`; the window's width, not the device's).
- Content column: 720 pt for lists and chat, 400 pt for forms, centred. The
  header's controls share the column; its border spans the window.
- Safe areas: the scaffold root pads all four edges with `pt/pl/pr/pb_safe_area`;
  the chat frame does the same on the web.

## Touch targets, surfaces, states

- Every control is at least 44 pt tall (`ui::TOUCH_H`), also with a mouse.
  Header bar 52 pt. List rows at least 52 pt; the whole row presses.
- Cards: card token, 1 pt border in the border token, the radius, no shadow.
  Rows separate with a 1 pt bottom border.
- Hover (pointer): accent fill for ghost, outline, link and rows; opacity 0.9
  or 0.92 for the filled variants. Pressed: opacity 0.75 (rows: muted fill).
  Focus: 2 pt ring in the ring token, on keyboard focus only (`focus_visible`;
  `focus_within` for a field). Disabled: opacity 0.4 for buttons, 0.5 for
  rows. Destructive actions use the destructive token (`Variant::Destructive`).

## Components (`app/src/ui.rs`)

From `nsh_base`: `Button`, behind `ui::button` and `ui::action`
(`nsh_base::button()`). Its dependency is declared in `app/Cargo.toml` with
default features off. The terminal already compiles `nsh-base` with its
defaults through `nsh-platform-tui`, so the terminal binary gains nothing;
the web target gains no feature (no QuickJS).

Composed here (no `nsh_base` equivalent): Card (`card`, `card_list`,
`card_header`, `card_title`, `card_description`, `card_content`,
`card_footer`), Separator, Badge, Alert, Skeleton, Label (the caption over a
field), the form field (`text_field`, with description), the header (`Bar`),
the list row and the empty state. Text input is NSH's own `input()` (which
`nsh_base` re-exports), styled by `text_field`. The filter field
(`search_field`) is a display field: the page's keys type into the filter,
so a real input there would take the keys twice.

Not yet adopted (no screen needs them): `nsh_base` Dialog, Sheet, Select,
Switch, Tabs, Tooltip, Checkbox. When a screen needs one, wrap it the same
way in `ui.rs`.

- `screen(Bar, body)`: `Bar::new(title)` with `.back(nav, tx)` (the Esc
  action) and `.action(element)`; `Bar::blank()` for no header (sign-in);
  `.form()` for the 400 pt column.
- `button(label, Variant, Size, press)`: Variant `Default`, `Secondary`,
  `Outline`, `Ghost`, `Destructive`, `Link`; Size `Sm`, `Default`, `Lg`,
  `Icon`. Chain `.w_full()` and `.disabled(flag)`.
- `action(label, tx, step)`: a Ghost button that sends a `UiAction`.
- `text_field(label, description, input)`; `""` leaves a line out.
- `search_field(value, placeholder)`.
- `badge(label, Tone)`, `alert(Tone, text)`, `skeleton(height)`,
  `separator()`, `status_dot(tone)`, `empty_state(title, line, action)`.
- `list_row(title, subtitle, trailing, press)`: `.disabled(flag)` stops the
  press; `.style_when(.., |s| s.opacity(0.5))` dims a row whose press the
  navigation decides on (the portal's offline rows).
- Tones: `Tone::{Success, Muted, Destructive, Warning}`.

## Rules for screens

1. Wrap every non-chat screen in `ui::screen` (through `scaffold` or
   `portal_bar` in `screens.rs`). The title is the screen's own word, moved
   from the body into the header.
2. Compose the components; do not style a control by hand. A look missing
   from `ui.rs` becomes a variant there, first.
3. Words, roles, placeholders and semantics are shared with the terminal
   (AGENTS.md "UI/UX parity contract"). Presentation only.
4. A style goes behind `WEB_CONTROLS`, or inside a component only drawn on
   the web. Never add a wrapper node on the terminal path.
5. Check with `node shots.mjs <dir>` (README) in four variants.

## Known gaps

- Chat (`chat.rs`): the transcript and composer are not restyled; only the
  status header and status line are inset, and the frame has safe areas.
- Settings, credentials, help, models, automations, tasks, cloud and the
  local sessions list use the old text rows inside the scaffold.
- The soft keyboard cannot be driven from Playwright: the sign-in card is
  centred and scrolls, but its behaviour with a real keyboard is unverified.
