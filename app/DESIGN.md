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

From `nsh_base` (UX-3): `Switch` behind `ui::switch` (a 44 x 24 track, the
thumb moves with the checked state, a 44 pt hit slop) and `Tab` / `Tabs`
behind `ui::tab` and `ui::tab_list`. Not yet adopted (no screen needs them):
`nsh_base` Dialog, Sheet, Select, Tooltip, Checkbox. When a screen needs one,
wrap it the same way in `ui.rs`.

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
- Settings and list rows (UX-3): `row(content, trailing, press)` is a 52 pt
  pressable row whose press covers the content only, so its trailing control
  (a destructive small button, a switch) takes its own presses;
  `static_row(content, trailing)` does not press. `value_pair(label, value)`
  is the label on the left and the value on the right. `choice_row(mark,
  current, selected, dim, label, press)` is a picker row: the current value
  carries the mark (the shared dot), an unavailable row is dim.
- `section_title(line)` (13 pt medium, muted, above a card), `body(line)`
  (16 pt, the foreground token), `field(label, value, active)` (a display
  field for an inline form, ringed when it is the active field).
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

## Chat (web, UX-2): `app/src/ui_chat.rs`

The chat draws its own bar and dock on the web (`chat::screen` and
`screens::chat_frame` branch on `WEB_CONTROLS`; the terminal keeps its header,
status line, strips and composer row). Words, roles, placeholders, keys and
order are the shared ones. Layout, phone first:

- Bar, 52 pt, border under it, inside the top safe area. Left: `sessions`
  (compact ghost). Middle: `modron` over a status row (dot and connection
  word). Right: `settings` and `help` (compact ghosts). The middle block is one
  44 pt press target: a tap opens a details row under the bar with the
  connection address and `session <id>`.
- Banner under the bar, only while not connected: `connecting` (warning alert
  with a skeleton line) or `disconnected` (destructive alert).
- Transcript, the virtual list at the page gutter, 720 pt column on wide
  windows. Tasks are secondary-token cards (the first and last rows of a run
  carry the card's padding). Assistant text is body 16 pt, 1.5 leading,
  paragraphs by the core's blank lines; a fenced code block is a bordered mono
  panel that scrolls sideways (the fence lines are markup and are not shown).
  The turn's tool summary is a compact bordered row, pressed to toggle the
  detail panel; its detail rows are mono and indented. Model and usage notes
  are centred captions. Shell output is mono. The in-flight text carries an
  accent rule and commits with the turn; opted-in reasoning is muted italic
  under a neutral rule. Both are web rows appended in `Model::from_snapshot`
  (`Style::Stream`, `Style::Reasoning`), so the terminal's rows are unchanged.
- Dock, pinned to the bottom above the safe area, border on top, page gutter:
  1. status strip, 13 pt caption (below);
  2. the core's error as a destructive alert; notes as captions;
  3. staged images as chips with `remove`;
  4. queued messages: a popover card, one 52 pt row each (a tap removes it);
  5. suggestions: a popover card, five rows, the selected one marked, a tap
     completes it;
  6. the approval sheet: the tool, its summary, and `allow once` (default),
     `allow all` (secondary), `deny` (destructive), full width in a row; the
     plan sheet: its lines and `approve` (default) and `revise` (outline). Both
     use the shared guard (`chat::prompt_guard_open`, 300 ms);
  7. the composer: a bordered field, 16 pt, grows to six lines (24 pt each);
  8. a row under it: `queue` and `attach` (ghost), then `send` (default,
     disabled while empty) or, while a turn runs, `stop` (outline).

Where the status surface's facts live:

| Fact | Web |
|---|---|
| connection state | bar (dot and word) |
| address, session id | details row (tap the bar's title block) |
| running or idle, spinner | status strip, first |
| model and effort, cumulative tokens, cache %, approval mode, phase, context | status strip (the core's status line, one caption) |
| provider | status strip |
| hint (esc to interrupt, or the idle hint) | status strip |
| goal | status strip |
| turn and tool counts | not shown: the core's status line has no turn count (the turn's tool summary row shows the tools) |

Chat components: `ui::button_text` (a reactive label, for the core's option
words; `ui::button` wraps it), and in `ui_chat`: `bar`, `banner`, `cell`,
`dock`, `summary_row`, `transcript_px`, `blocks` (the fence parser, tested).

Layout limits of NSH 0.5.1 (see the code comments):

- `LineHeight` is not publicly re-exported, so leading is a point value
  computed from the font size.
- A virtual list needs an explicit height, and the popovers are in the flow,
  not floated: `ui_chat::transcript_px` estimates the dock's height from the
  state (popovers, composer lines, approval, plan). Re-measure after a change
  to the dock.
- `ui::list_row` always truncates its title, so a long suggestion shows an
  ellipsis (the full text is in the page).

## Settings, credentials, pickers, lists, help, Cloud (web, UX-3): `app/src/ui_screens.rs`

The bodies of the non-chat screens on the web. `screens.rs` chooses between
these and the terminal's text bodies on `WEB_CONTROLS` (the terminal's bodies
are unchanged). Phone first; each screen is wrapped in `ui::screen` at 400 pt
(forms: settings, credentials, pickers, models, help, Cloud) or 720 pt (lists:
sessions, tasks, automations).

- Settings: the rows as one card; a row's label on the left and its value on
  the right; removable rows carry a destructive small control that asks first
  (the confirmation is drawn). The notify row is a static row with a switch
  (disabled when it cannot be turned on); its hint and note sit under it. The
  share section: the two invite roles (outline), the new link in a muted
  monospace panel that scrolls sideways with a copy control, the invites in a
  card with a destructive revoke each.
- Credentials: the provider rows in a card (label left, status right; a
  signed-in row has a remove control); the browser sign-in progress in a card;
  the masked key entry in a card. The sign-in's Esc hint is not drawn (the
  back control does the same).
- Pickers (`picker`, `models`): the filter as a search field, the rows in a
  card, the current value marked with the shared dot, an unavailable row dim.
  `models` has the save-as-default control (outline).
- Lists (sessions, tasks, automations): the filter as a search field, rows in a
  card with their own control (delete, merge), the create control full width,
  the inline form as a card of fields with save (default) and cancel (outline).
  The form's keyboard hint is not drawn.
- Help: the three tabs as a segmented control (`tab_list`), the rows in a card,
  a footer caption for the rows shown.
- Cloud: the accounts card (remove asks first), the token entry card (masked),
  the new-run card (account and repo pickers as outline buttons, the branch
  field, start as a default button), the runs card (open outline, stop
  destructive, wake secondary). The repo picker is a search field and a card.
- Agent panel (web): the row and its kill control in a card, above the
  transcript.

Words stay the shared ones. Keyboard-only hints are not drawn on the web where
a visible control does the same: the form hints, the key hint on the save
controls (the web uses the `save` word), the sign-in wait line's Esc hint.
Their constants stay in the shared table.

## Known gaps

- The soft keyboard cannot be driven from Playwright: the sign-in card is
  centred and scrolls, but its behaviour with a real keyboard is unverified.
- The terminal bodies that the web no longer draws (the old text rows in
  `screens.rs`) remain as the terminal's path; a few of their web-only branches
  are dead on the web and are kept for the terminal's shape.
