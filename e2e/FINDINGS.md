# End-to-end findings

`node e2e/run.mjs` drives a real `modron serve` (mock provider) through
the real bridge Worker (local wrangler dev) to the real web app in
headless Chromium.

## 2026-11-15, first run: 2 of 11 passed

| ID | Finding | Status |
|---|---|---|
| F1 | The remote-control tunnel never subscribed to its session. It wrote its own `hello` and `attach` into the bridge sink (so viewers received them as agent frames) instead of dispatching them, so no snapshot, event or approval ever reached a viewer that did not attach by itself. | Fixed in crates/modron-server/src/rc.rs: both frames go through `handle_frame`. |
| F2 | The share URL kept the `ws://` / `wss://` scheme of the bridge base, which a browser cannot open. | Fixed: the viewer page URL is always http(s). |
| F4 | A late viewer got no history when the snapshot exceeded the relay's 96 KB buffer limit (181 KB observed). | Fixed on the server: every relayed `hello` is answered with a fresh snapshot of the tunnel's session, so history no longer depends on the relay buffer. |
| F3 | The web app logs "closure invoked recursively or after being dropped" on load. | Not reproduced after E2E-fix; see below. |

## After the fixes: 9 of 11 pass

Open:

- S4 (stop from the browser): the stop control was not clickable within
  15 s. Not yet diagnosed: the control may not be shown for a run that
  is waiting on an approval, or the mock run may end too quickly.
- S7d (approver invite approves): the approval control was not
  clickable within 15 s for an approver-role viewer. Not yet diagnosed.

Evidence for each run is written to the run's temp directory
(evidence-<id>.json and screenshots); see README.md.

## E2E-fix, 2026-11-15: 11 of 11 pass

Every viewer's console errors and page errors now fail the step that
observed them (run.mjs `checkConsole`), and web-test.js cases (a) and (b)
assert no page or console errors.

### S4 (stop not clickable): product defect, layout

Root cause: the chat body put a fixed 600 px virtual list above the
approval strip, queue/stop row and composer (app/src/chat.rs, the
`.h(px(600.0))` on the transcript). The page is `html, body { overflow:
hidden }` (packaged index.html), so nothing scrolls. At the Playwright
default viewport of 1280x720 the approval options sat at y=719 and the
queue/stop row at y=736, outside the viewport. Playwright refused the
click ("element is outside of the viewport"). The run was in fact
active: the Stop control was rendered and the snapshot said running
while the approval was pending.

Fix: the transcript height follows the window (app/src/web.rs sends
`innerHeight` on change over a channel; app/src/lib.rs drains it into a
signal; app/src/chat.rs `fit_transcript_height`: window minus 330 px,
clamped to 240..600). At 720 px: allow once y=509, stop y=543, click
lands on the stop span. Unit test: chat.rs
`transcript_leaves_the_controls_on_a_short_window`.

Note for later: a signal may not be set from the page timer (outside the
engine's reactive runtime): nsh panics with "no reactive runtime". The
channel avoids that.

### S7d (approver approve): same root cause

The approver invite page has the same bottom rows, so "allow once" was
clipped by the same fixed height. S7d passes after the layout fix. I did
not capture the pre-fix approver page, so this attribution rests on the
identical symptom and the S4 measurement, not on a before/after capture.
The bridge's PERMITTED_FRAMES were not the cause: the approver's
`approve` frame was forwarded and resolved the approval (the scenario
checks approval_resolved on the local side).

### F3 (wasm closure message): not reproduced

No "closure invoked recursively" message appears with the current build
in any run (11 e2e scenarios, web-test.js a to k, console errors captured
for every viewer). The message is wasm-bindgen's report for a Closure
called after a panic left its borrow flag set, so it is a symptom of an
earlier panic inside a web closure (web.rs tick or keydown listener).
The first panic is not identified. The two closures keep `forget()`
because they live for the page's life (one per page, not per render).
Action if it returns: capture the console message location
(`m.location()`) and the first pageerror before it.

### Left for the lead

None in crates/ or mobile/ were needed for these three items. The app
build consumes crates/modron-view, which was briefly mid-edit (a
non-exhaustive match on ToolActivity); the bundle was built once it
compiled and includes the in-progress crate changes present at that time.
