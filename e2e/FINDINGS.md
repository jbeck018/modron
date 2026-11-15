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
| F3 | The web app logs "closure invoked recursively or after being dropped" on load. | Open. It did not block sending once F1 was fixed. |

## After the fixes: 9 of 11 pass

Open:

- S4 (stop from the browser): the stop control was not clickable within
  15 s. Not yet diagnosed: the control may not be shown for a run that
  is waiting on an approval, or the mock run may end too quickly.
- S7d (approver invite approves): the approval control was not
  clickable within 15 s for an approver-role viewer. Not yet diagnosed.

Evidence for each run is written to the run's temp directory
(evidence-<id>.json and screenshots); see README.md.
