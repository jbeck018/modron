# Modron app parity audit (old terminal UI retirement)

Packet PARITY. Read-only audit. Date 2026-11-15. No cargo run, no commit.

## Scope and state

- Old TUI: `crates/modron-tui/` (all `src/...` citations below are relative to it).
- New app: `app/` (`app/src/...`, `app/tests/...`, `app/README.md`, `app/NOTES.md`).
- Client core: `mobile/rust-core/src/` (`core/client.rs`, `core/view.rs`).
- Protocol: `crates/modron-protocol/src/lib.rs` (`proto`). Shared view and slash table: `crates/modron-view/src/` (`view`). CLI: `crates/modron-cli/src/` (`cli/`).
- The old TUI is unchanged since 3afd1f5. While the audit ran, another agent had uncommitted owner-portal edits in `app/src/chat.rs`, `app/src/nav.rs`, `app/src/screens.rs`, `app/src/session.rs`, `app/src/lib.rs`, `mobile/rust-core/src/view.rs` and the untracked `app/src/portal.rs`. Those edits were committed as a17642c during the audit. The statuses reflect the files as read before that commit; `app/src/bin/modron-app-tui.rs` (the terminal key map) was not changed by them.
- Status words: DONE (works in the app; cited), PARTIAL (present but incomplete or different), CORE-ONLY (the client core supports it and no app screen or key does), MISSING (neither), NOT-APPLICABLE (replaced by design or nothing visible to match; reason given).
- Anything not read directly is marked (inferred).
- Not measured: startup time. The terminal host was not run.

## Summary

Counted rows (the seven tables below): 140 rows. DONE 52, PARTIAL 17, CORE-ONLY 15, MISSING 50, NOT-APPLICABLE 6. The old TUI cannot be deleted yet. Several keys are dropped at the terminal host's key map (Tab, F1, Left and Right, Home and End, Delete, PgUp and PgDn, Shift+Tab); the host never enables bracketed paste, so multi-line paste submits line by line (inferred); newline entry is impossible; Esc on an approval interrupts the run; and the app lacks input history, `@file`, `!command`, image attachments, a credential screen, and the tool detail panel. None of the must-haves needs a new protocol frame; three need core work (attachments on submit, a per-event tool list in the snapshot, a unix-socket transport). The rest is app work.

## Keys

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Enter sends the task | src/lib.rs:1116-1157 | DONE | chat.rs route_key sends Enter to the composer; screen() on_submit sends Send (chat.rs:1253); app/tests/chat.rs typing_and_enter_submits_and_keeps_focus. |
| Enter while a turn runs | src/run.rs:913-916 (refused with a toast) | DONE | Steers the running turn through core send_message (core/client.rs:605). Behaviour differs from the old refusal. |
| Shift+Enter or Alt+Enter inserts a newline | src/lib.rs:1103-1115 | MISSING | Alt+Enter queues here (chat.rs route_key Enter arm). Shift is ignored, so Shift+Enter submits. The composer is NSH SubmitBehavior::Submit (chat.rs:1246), which inserts no line breaks (nsh-core ops.rs:1381-1389). |
| Ctrl+J inserts a newline | src/lib.rs:942-945 | MISSING | Ctrl chords pass to the composer (chat.rs route_key chord arm); no newline path exists. |
| Backslash then Enter continues the line | src/lib.rs:1117-1125 | MISSING | No code; Enter submits. |
| Up/Down recall sent inputs (single line) | src/lib.rs:1206-1213, 1935-1959 | MISSING | Up/Down fall through to the composer (chat.rs route_key); the app keeps no input history. |
| Up/Down move between lines of a multi-line input | src/lib.rs:1198-1205, 1963-1977 | MISSING | No multi-line input (see the Shift+Enter row). |
| Left / Right move the cursor | src/lib.rs:1179-1186 | MISSING | app/src/bin/modron-app-tui.rs:48-62 maps only Char, Enter, Esc, Backspace, ArrowUp, ArrowDown and F2; other keys become "Unknown". NSH names ArrowLeft and ArrowRight (nsh-base rich_editor/view/keys.rs:35, 164). |
| Home / End | src/lib.rs:1187-1194 | MISSING | Same key-map gap (bin:48-62). NSH names Home and End (keys.rs:39-40). |
| Delete | src/lib.rs:1169-1178 | MISSING | Same key-map gap (bin:48-62). |
| Backspace edits; on an empty composer it removes the last queued message | src/lib.rs:1158-1168 | DONE | bin:53 maps Backspace; chat.rs route_key dequeues when the draft is empty and messages are queued (a new behaviour). |
| Esc interrupts a running turn | src/lib.rs:1048-1050 | DONE | chat.rs route_key maps Escape to Interrupt (chat.rs:405); app/src/session.rs apply sends Interrupt to core interrupt (core/client.rs:545). |
| Esc when idle: clear selection, leave scrollback, clear input | src/lib.rs:1240-1251 | MISSING | Idle Esc sends Interrupt (chat.rs:405). Nothing clears the composer. |
| Esc closes the open screen | src/lib.rs:1006-1011, 1029-1036 | DONE | chat.rs route_screen_key handles Esc for Help, Agent, Models, Settings and Sessions. The tool panel and auth manager are MISSING and CORE-ONLY (see the screens table). |
| Esc on a pending approval denies it | src/lib.rs:984-986 | PARTIAL | chat.rs:405 returns Interrupt for Esc before the approval block, so Esc interrupts the run instead of denying the prompt. n denies (chat.rs DECISION_KEYS). (inferred: the interrupt ends the whole run, not just the prompt.) |
| Ctrl+C aborts a running turn; quits when idle | src/lib.rs:949-956 | PARTIAL | bin:255-261 exits on Ctrl+C at any time, with no abort first. An owned server is stopped on exit (app/src/startup.rs:303-364). |
| Ctrl+O toggles transcript detail (compact or full) | src/lib.rs:925-928, 3089-3098 | MISSING | Ctrl+O opens Sessions (chat.rs route_key, `Key::Char('o') if chord`). The old toggle changed only a status label. |
| Ctrl+T expands this turn's tool activity | src/lib.rs:930-938, 3382-3409 | MISSING | No tool detail panel. The snapshot carries rendered tool lines only (core/view.rs:682-764). |
| Ctrl+V pastes an image as an attachment, or text into the input | src/lib.rs:946-948; src/clipboard.rs:55-64 | MISSING | No clipboard code in app/. Ctrl+V reaches the composer as a chord, which the app does not handle (inferred: no effect). |
| Pasted text enters the input verbatim, including multi-line | src/run.rs:807-815 (bracketed paste enabled at src/run.rs:173-176) | PARTIAL | bin:274 handles Event::Paste, but the terminal host never enables bracketed paste (none in app/ or nsh-platform-tui-0.5.1). Multi-line paste therefore arrives as Enter presses and submits each line (inferred). |
| Shift+Tab flips the approval mode | src/lib.rs:1229-1239 | MISSING | BackTab is not mapped (bin:48-62). /mode and the Settings approval row work (slash.rs run; nav.rs settings_rows). |
| PgUp / PgDn page the scrollback | src/lib.rs:1215-1224 | MISSING | Not mapped (bin:48-62); app/src/chat.rs has no keyboard scroll. |
| Mouse wheel scrolls the scrollback | src/run.rs:837-838 | MISSING | No mouse events are handled, and the terminal host never enables mouse capture (bin has no EnableMouseCapture). |
| Click positions the input cursor | src/lib.rs:1997-2043 | MISSING | Same as the mouse wheel row. |
| Drag selects scrollback text; release copies it | src/run.rs:816-835; src/lib.rs:2067-2149; src/clipboard.rs:27-52 | MISSING | No mouse handling. Native terminal selection still works because mouse capture is off (inferred). |
| Click a tool summary or the tool strip opens the panel; click outside closes it | src/lib.rs:1997-2013, 2067-2072 | MISSING | No tool panel (see the Ctrl+T row). |
| Tab completes the highlighted /command | src/lib.rs:1068-1072 | PARTIAL | chat.rs route_key maps Tab to CompleteSelected, but the terminal host does not map Tab (bin:48-62), so Tab never arrives. Enter completes a partial command instead. |
| Up/Down move the highlight in the command list | src/lib.rs:1059-1066 | DONE | chat.rs route_key (Up/Down while suggesting); bin:54-55 maps ArrowUp and ArrowDown. |
| Tab completes an @path | src/lib.rs:1094-1097 | MISSING | No @ completion (see the input prefixes table). |
| ? on an empty input opens help | src/lib.rs:1148-1150 | DONE | chat.rs route_key opens Help for Char('?') when the draft is empty. |
| y / a / n decide the approval; a allows all for the run | src/lib.rs:974-987 | DONE | chat.rs route_key approval block (DECISION_KEYS); labels from core/view.rs:59-64; app/tests/chat.rs a_during_pending_approval_decides_option_one. |
| Approval type-ahead guard: decision keys typed as the prompt appears are ignored | src/lib.rs:962-973 | MISSING | No equivalent in app/src/chat.rs or app/src/session.rs. |
| Shift+S on a model row saves it as the default | src/lib.rs:1553-1573; src/run.rs:1593-1601 | CORE-ONLY | core/client.rs:949 persist_router exists; the app has no key or screen for it. |

## Slash commands

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| /help and bare ? | src/lib.rs:2481-2488, 186 | DONE | slash.rs run "help" opens the Help screen (screens.rs help_body, 330); the key table is app/src/help.rs; shared lines from view slash.rs:140-146. |
| /exit, /quit, /q | src/lib.rs:2480, 187-189 | MISSING | No arm in app/src/slash.rs run; the input falls to the unknown-command notice. Ctrl+C exits. |
| /clear (empties the scrollback) | src/lib.rs:2489-2500, 190 | MISSING | No arm; nothing clears the transcript in app/. |
| /compact | src/lib.rs:2501-2508, 191 | DONE | slash.rs "compact" -> UiAction::Compact -> session.rs client.compact (core/client.rs:705). |
| /model [name] | src/lib.rs:2509-2529, 1643-1676 | PARTIAL | slash.rs:113-114 opens the picker when bare and switches to a named model otherwise. The picker lacks a current marker and save-default (see the Shift+S row). |
| /context | src/lib.rs:2530-2536 | PARTIAL | Only the status-line context percentage (core/view.rs:90-103, 790-795). The old token breakdown line is absent. |
| /sessions | src/lib.rs:2537-2540 | DONE | slash.rs "sessions" opens the Sessions screen (nav.rs ListSessions). The old command printed a pointer to the CLI. |
| /sandbox [off, seatbelt, bwrap] | src/lib.rs:2541-2550, 1711-1737 | PARTIAL | slash.rs "sandbox" cycles when bare (no picker). It rejects "bubblewrap", which the old parser accepted (src/run.rs:2486-2495). |
| /reasoning (and /thinking) | src/lib.rs:2551-2559, 2737-2741 | CORE-ONLY | core/client.rs:491 set_show_reasoning exists; core/view.rs:694-697 carries streaming_reasoning. The app has no command or display. |
| /btw <question> | src/lib.rs:2560-2569 | MISSING | No arm in slash.rs. App work: submit with the no-tools rider. |
| /agent [name] (and /persona) | src/lib.rs:2583-2590, 1763-1784 | CORE-ONLY | proto ConfigPatch.persona (proto lib.rs:548-580) and core/client.rs:732 set_config can carry it. The app has no command and no persona list. |
| /allow [tool], /deny [tool] | src/lib.rs:2591-2606, 1787-1820 | CORE-ONLY | ConfigPatch allow and deny (proto lib.rs:548-580); snapshot allow_tools and deny_tools (core/view.rs:723-724). No app command. |
| /mode [ask, yolo, deny] | src/lib.rs:2607-2614, 1740-1760 | PARTIAL | slash.rs "mode" sets the mode and accepts deny. Bare /mode cycles instead of opening the old picker. |
| /tasks | src/lib.rs:2615-2618 | DONE | slash.rs "tasks" -> session.rs TaskStatus -> core/client.rs:910 task_status. |
| /worktree (and /worktrees) | src/lib.rs:2619-2622; src/run.rs:1698-1714 | CORE-ONLY | core/client.rs:897 create_task, :910 task_status and :919 merge_task exist. No app command and no worktree list. |
| /issues [add, done] | src/lib.rs:2623-2626; src/run.rs:1868-1936 | MISSING | No arm in slash.rs; no core call for issues (core/client.rs has none). |
| /fork | src/lib.rs:2627-2630 | DONE | slash.rs "fork" -> session.rs Fork -> core/client.rs:887 fork_session. |
| /plan <task> (plan-first task) | src/lib.rs:2655-2664 | MISSING | The old command submitted a plan-first task. The shared table now defines /plan as a phase switch (view slash.rs:91-96), and app/src/slash.rs:138 discards any argument, so a task typed after /plan is lost. |
| /review (and /code-review) | src/lib.rs:2665-2670, 212 | MISSING | No arm; the old command submitted a review-the-diff task. |
| /goal [condition] | src/lib.rs:2671-2685, 3022-3029 | CORE-ONLY | proto ConfigPatch.goal (proto lib.rs:567) can set it; core/view.rs:747 carries the goal in the snapshot. The app neither sets nor shows it. |
| /learn [title] | src/lib.rs:2686-2699 | MISSING | No arm; the old command submitted a distill-to-SKILL.md task. |
| /theme [name] | src/lib.rs:2700-2724; src/theme.rs:44-124 | MISSING | No theme switching; app/src/lib.rs install_theme always installs oculus. |
| /auth [args] | src/lib.rs:2570-2582; src/run.rs:1938-2001 | CORE-ONLY | slash.rs:111 maps /auth to the Settings screen, which has no credential controls. Core calls exist (core/client.rs:838-870). |
| /settings | src/lib.rs:2570-2582, 262 | PARTIAL | slash.rs:111 opens the new Settings rows (nav.rs settings_rows, 641): model, approval, phase, effort, sandbox and compact act; provider, context and rules are read-only. The old /settings opened the auth manager. |
| /new | src/lib.rs:2651-2654; src/run.rs:1490-1496 | DONE | slash.rs "new" -> session.rs NewSession -> core/client.rs:875 new_session. |
| /share | src/lib.rs:2639-2650 | DONE | slash.rs "share" -> session.rs Share -> publish_page with the whole transcript. The old command published only the last answer. |
| /remote-control [url] | src/lib.rs:2631-2634; src/run.rs:1049-1057 | DONE | slash.rs:143 gives a usage line when bare; session.rs RemoteControl -> core/client.rs:815 remote_control. |
| /automations | src/lib.rs:2635-2638 | CORE-ONLY | core/client.rs:761-795 list, add and remove automations. No app screen. |
| Unknown command | src/lib.rs:2726-2729 | DONE | slash.rs run returns the shared unknown-command notice (core/view.rs:158). |
| Aliases: /? /compress /login /publish /rc /thinking /persona /worktrees /code-review | src/lib.rs:185-227 (aliases in CATALOG and parse) | MISSING | slash.rs run matches only the new names; these fall to the unknown-command notice. |

## Input prefixes

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| !command shell passthrough (runs in the workspace, or on the client when remote) | src/run.rs:871-908, 1839-1841 | MISSING | No "!" handling in app/src (chat.rs route_key, session.rs apply). |
| !command output: stdout, stderr, exit code; capped at 10 KiB | src/run.rs:1307-1327, 1847-1857 | MISSING | Depends on the row above. |
| @path mention: popup, Up/Down, Tab completes | src/lib.rs:1077-1100, 2170-2248, 3706-3730 | MISSING | No file walk or popup in app/. |
| Image attachments (Ctrl+V) and attachment chips | src/lib.rs:2984-3018, 3778-3787 | MISSING | No attachments in app/. The core submit sends empty attachments (core/client.rs:529-545). The TUI's remote driver sends attachments in Payload::Submit (src/run.rs, spawn_remote_driver), so the wire carries them (inferred). |
| Dropped file paths arrive as text | src/run.rs:807-815 | DONE | Both hosts take a dropped path as plain text and attach nothing (inferred). |

## Screens and popups

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Scrollback of committed transcript, wrapped | src/lib.rs:2265-2296, 3308-3325 | DONE | chat.rs transcript_lines (654) styles each core history row (core/view.rs:356-390). |
| Scrollback paging and the "lines above" indicator | src/lib.rs:1215-1224, 3309-3316 | MISSING | No keyboard paging and no mouse scroll (see the Keys table). |
| Live answer tail while streaming | src/lib.rs:3288-3295, 2734-2745 | DONE | chat.rs screen() draws core streaming_text (core/view.rs:693). |
| Live reasoning tail (opt-in) | src/lib.rs:3279-3287, 2737-2741 | CORE-ONLY | The core carries streaming_reasoning and show_reasoning (core/view.rs:694-697); no app display. |
| Live tool strip: turn summary and latest rows | src/lib.rs:3411-3429 | DONE | chat.rs tool_strip (1079) shows core turn_summary_line and tool_lines (core/view.rs:699-701); last 6 rows (chat.rs:212). |
| Expanded tool panel: all events, scroll, Esc closes | src/lib.rs:3382-3409, 2825-2830 | MISSING | No panel. The snapshot has no per-event list; core/view.rs:543 holds turn_tools on the view model but the snapshot omits it. |
| Sub-agent rows above the input | src/lib.rs:3329-3380, 2762-2793 | DONE | chat.rs swarm_slots (1324) renders core subagent_rows (core/view.rs:703). |
| Approval strip: tool, args summary, three options | src/lib.rs:3431-3440 | DONE | chat.rs approval_strip (1153) renders the core pending approval and its labels. |
| Command list above the input: filtered, windowed | src/lib.rs:3669-3705 | DONE | chat.rs slash_list (1358) renders slash.rs suggestions_for. |
| Model picker: filter, current marker, provider and window detail, no-key marking | src/lib.rs:1643-1676, 3584-3630 | PARTIAL | nav.rs model_rows (602) lists the server catalog once request_models answers (session.rs apply ListModels). No current marker and no preselect. Unavailable rows look the same (screens.rs model_row, 298) and Enter ignores them (nav.rs apply, PickModel). |
| Theme picker | src/lib.rs:1679-1707 | MISSING | See /theme. |
| Sandbox picker | src/lib.rs:1711-1737 | MISSING | Bare /sandbox cycles instead. |
| Mode picker | src/lib.rs:1740-1760 | MISSING | Bare /mode cycles instead. |
| Persona picker | src/lib.rs:1763-1784 | MISSING | No persona list in the snapshot (core/view.rs:682-764). |
| Allow and deny tool picker | src/lib.rs:1787-1820 | MISSING | No tool list in the snapshot; only the allow and deny lists (core/view.rs:723-724). |
| Auth manager: provider picker, method choice, masked key entry, browser or device sign-in wait, remove | src/lib.rs:416-496, 1264-1485, 3448-3581 | CORE-ONLY | Core auth calls (core/client.rs:838-870) and snapshot providers and oauth progress (core/view.rs:591-605, 752-761) exist; no screen. |
| Help modal: three tabs (General, Commands, Shortcuts), scroll | src/lib.rs:275-412, 3635-3667 | PARTIAL | screens.rs help_body (330) is one page built from app/src/help.rs and the shared slash lines. No tabs and no scroll. |
| Toast strip: transient notes that expire after 40 ticks | src/lib.rs:905-908, 2410-2439, 3732-3745 | PARTIAL | chat.rs note_rows (1072) shows the core last note and last error until replaced. Nothing expires (chat.rs:216 NOTE_SLOTS = 2). |
| Input box title "working (esc to interrupt)"; rotating placeholder hints | src/lib.rs:3747-3823 | PARTIAL | The composer uses the shared COMPOSER_PLACEHOLDER (chat.rs:1247). No interrupt copy in the terminal and no rotating hints. |
| Input box caret and scrolling input | src/lib.rs:1878-1930, 3844-3857 | PARTIAL | NSH input draws the caret and edits single-line text (inferred). Multi-line input and Left/Right are missing (see the Keys table). |
| Generative UI blocks rendered as aligned ASCII | src/lib.rs:2881-2898 | DONE | core/view.rs:389 renders GenerativeUi entries with modron_view::render_ascii; the app shows the core lines. |
| Turn-end usage line (tokens, turns, cache) | src/lib.rs:2907-2918 | DONE | Core Usage entry; chat.rs style_of gives it the meta style (inferred: no visual check). |
| Swarm summary at turn end | src/lib.rs:2762-2793 | DONE | Core Swarm entry; chat.rs style_of gives it the tool style (inferred). |
| Tool-activity summary line committed per turn | src/lib.rs:2799-2821 | DONE | Core ToolSummary entry; chat.rs style_of gives it the tool style (inferred). Click-to-expand is MISSING (see the expanded tool panel row). |
| Interrupted-turn note | src/lib.rs:2925-2940 | DONE | Core records an abort summary as last_note (core/client.rs:918 doc comment); the app shows last_note (inferred). |
| Welcome banner | src/run.rs:541-575 | MISSING | No banner in app/src/chat.rs or screens.rs. |
| Update notice | src/lib.rs:588 (field only; view() never reads it) | NOT-APPLICABLE | The old TUI never draws the notice (the field is set only at src/run.rs:274-321), so there is no visible behaviour to match. The CLI check remains (cli/main.rs:2915). |
| Context-meter bar | src/lib.rs:3863-3875 | NOT-APPLICABLE | Only tests call context_meter (crates/modron-tui/tests/state.rs:399-404). The rendered status shows the percentage only (src/lib.rs:3059-3069). |

## Status surface

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Model label | src/lib.rs:3048 | DONE | Core status line starts with the model (core/view.rs:768-772). |
| Cumulative tokens | src/lib.rs:3049 | DONE | core/view.rs:780. |
| Session cache hit percentage | src/lib.rs:3054-3056 | DONE | core/view.rs:781-783. |
| Context percentage with the 80 percent warning | src/lib.rs:3057-3069 | DONE | Server gauge through core/view.rs:90-103 and 790-795. The old ctx segment never received context_used outside tests (src/lib.rs:594), so it always read 0 percent. |
| Approval mode | src/lib.rs:3070-3073 | DONE | core/view.rs:784. The "(shift+tab)" hint is absent (see the Shift+Tab row). |
| Spinner glyph while running | src/lib.rs:3041-3045 | PARTIAL | The header shows the words "running" or "idle" (chat.rs header, 964; core/view.rs:800). No animated glyph. |
| Turn number while running | src/lib.rs:3074-3078 | CORE-ONLY | snapshot.turns_completed (core/view.rs:743) exists; the app does not use it. |
| Current tool name while running | src/lib.rs:3079-3084 | DONE | The tool strip shows the latest tool row (chat.rs tool_strip, 1079). |
| "esc interrupt" hint while running | src/lib.rs:3085 | MISSING | No such text in app/src. The only interrupt strings are the help row (help.rs:65) and code. |
| Idle hints: "? shortcuts", "ctrl+o compact/full", "esc clears" | src/lib.rs:3086-3102 | MISSING | screens.rs:35 shows "ctrl+o sessions   f2 settings   f1 help". "?" is not hinted, and F1 cannot be pressed in the terminal (bin:48-62). |
| Session id (first 8 characters) | src/lib.rs:3126-3130 | MISSING | The session id is stored (nav.rs:321) and never shown. |
| Approval prompt in the status line | src/lib.rs:3134-3144 | DONE | The approval strip (chat.rs:1153) shows the tool and its args summary. |

## Local behaviours

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Engine runs in-process with no server | src/run.rs:304-308, 443-474 | NOT-APPLICABLE | Replaced by design: the app is always a client and starts its own `modron serve` child (app/src/startup.rs:303-364; app/README.md). It needs a modron binary (startup.rs:174 find_bin) and waits up to 5 s (startup.rs:25). |
| First run with no key: onboarding, then mock fallback | cli/main.rs:734-771; cli/onboarding.rs:88 | CORE-ONLY | Core has auth_set_key and auth_providers (core/client.rs:838-870). The app has no onboarding and no key check. (inferred: the server's no-key behaviour is not checked.) |
| Session resume: --resume (bare = newest session; =id) | cli/main.rs:363-366, 1185-1190 | DONE | app/src/startup.rs:61-70 accepts --session, --continue (newest in any folder) and --new. Default is the newest session in the current folder (session.rs pick_session, 256). The app has no --resume flag. |
| Connect target: --connect | cli/main.rs:368-372 | DONE | --url (startup.rs:66). Unix sockets are MISSING (see the remote table). |
| Rewind marker honoured on resume | cli/main.rs:192, 514-515, 1249-1252 | NOT-APPLICABLE | `modron rewind` is a CLI subcommand; the TUI has no rewind key or command. Whether modron serve honours the marker is not checked (inferred unknown). |
| Goal flag --goal | cli/main.rs:358-361 | MISSING | startup.rs parse_args (56-92) rejects unknown flags, and the app passes only provider and model to its child (startup.rs:320-325). |
| Approve-all startup flags (--yolo, --approve-all) | cli/main.rs:382-390 | MISSING | Not accepted by startup.rs parse_args. /mode yolo works at runtime (slash.rs). |
| Other startup flags: --sandbox, --effort, --agent, --allow, --deny, --working-dir, --sessions-dir, --max-cost, --max-turns, --verbose, --attach | cli/main.rs:343-416 | MISSING | None is accepted; the child gets --provider and --model only (startup.rs:320-325). `modron serve` takes these as global flags (inferred: forwarding them should work). |
| Custom themes from the themes directory, and built-in palettes | src/lib.rs:889-897; src/theme.rs:44-124 | MISSING | The app ships only the oculus tokens (app/src/lib.rs install_theme). |
| tui-dev hot reload | none | NOT-APPLICABLE | No such code: a repo-wide search for tui-dev and hot reload finds nothing in crates/modron-tui (the only "hot" words are comments at src/remote.rs:153). |
| TUI logs go to a file, not stderr | cli/main.rs:445 (init_tracing, called at 422) | DONE | The app writes the child server's log to $XDG_STATE_HOME/modron/app-serve.log (app/src/startup.rs:272-278). The client writes no log of its own (inferred). |
| Session id printed on exit, with the resume command | src/run.rs:400-405 | MISSING | app/src/bin/modron-app-tui.rs prints nothing on exit. |

## Remote behaviours

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Unix socket target (default ~/.modron/serve.sock; unix:<path>) | src/remote.rs:88-91, 160-195 | MISSING | The core is WebSocket only (core/client.rs:118-140), and the app accepts ws URLs only (startup.rs:66). Core work. |
| WebSocket to loopback (ws://) and wss:// | src/remote.rs:160-195 | DONE | core/client.rs:118-140 validates ws and wss; the app probes (startup.rs:235) and connects (session.rs). |
| Plain ws:// to a non-loopback host (LAN) | src/remote.rs:160-195 | CORE-ONLY | The core refuses it unless ConnectOptions.allow_insecure is set (core/client.rs:77-89, 135-137). The app never sets it and has no flag. |
| Device token: --token, ?token= in the URL, MODRON_DEVICE_TOKEN | src/remote.rs:36-80 | PARTIAL | --token works (startup.rs:67). ?token= and MODRON_DEVICE_TOKEN are not read: neither string appears in app/ or mobile/rust-core. |
| Protocol handshake; a mismatched server is refused | src/remote.rs:83-86 (client_hello) | DONE | core/client.rs:1216-1220 checks the Welcome protocol version. |
| Attach on connect; replay the transcript | src/run.rs:424-438, 582-601 | DONE | session.rs link_up (419-436); core history rows (core/view.rs:764). |
| Live event folding into the transcript | src/run.rs:2196-2400 (ReplayFolder) | DONE | core/view.rs:941 fold_event (inferred from the function's role). |
| Approval decisions over the wire | src/run.rs:86-89 (LoopCmd::Approve) | DONE | session.rs apply Decide -> core approve (core/client.rs:555). |
| Approval resolved elsewhere clears the prompt | src/run.rs:1375-1386 | DONE | The core keeps the pending approval in the snapshot and the app shows it (inferred: no app test covers it). |
| Submit and interrupt over the wire | src/run.rs:928-933, 958-968 | DONE | session.rs apply Send -> send_message; Interrupt -> interrupt. |
| /fork and /tasks over the wire | src/run.rs:1058-1063 | DONE | session.rs apply Fork and TaskStatus. |
| /share over the wire | src/run.rs:1064-1070 | DONE | session.rs apply Share -> publish_page. |
| /automations over the wire | src/run.rs:1071-1073 | CORE-ONLY | Core list, add and remove automations (core/client.rs:761-795); no app screen. |
| /new over the wire, with session adoption | src/run.rs:1074-1076 | DONE | session.rs apply NewSession. The core adopts the session a snapshot names (app/NOTES.md, Packet C3e; app/tests/live.rs). |
| /remote-control over the wire | src/run.rs:1049-1057, 2497-2510 | DONE | session.rs apply RemoteControl -> core remote_control (core/client.rs:815). |
| Mode, model and sandbox sent as config patches | src/run.rs:1077-1083, 2415-2470 | DONE | session.rs apply SetApprovalMode, SelectModel and SetSandbox. |
| Persona, allow and deny sent as config patches | src/run.rs:2415-2470 | CORE-ONLY | ConfigPatch persona, allow and deny (proto lib.rs:548-580). No app command. |
| Engine-local commands refused while connected, with a toast | src/run.rs:1085-1089 | NOT-APPLICABLE | The app has no engine-local commands to refuse: each command is a core call or a screen (app/src/slash.rs run). |
| Undecodable frame notice | src/remote.rs:109; src/run.rs:2219-2220, 2302-2305 | DONE | core/client.rs:1090-1092 and 1172 set the notice as last_note; the app shows last_note (inferred). |
| Connection header: state and address (old: "connected to" note) | src/run.rs:587-591 | PARTIAL | The header shows the core connection label and address (chat.rs header 964; session.rs label). The old "sessions survive this TUI" note is absent. |

## Before the old TUI can be deleted

Ordered by how often a daily terminal user hits the gap. "App" means app/ only; "core" means mobile/rust-core; "server" means the protocol or `modron serve`. No new protocol frame is needed by any item below.

1. **Terminal key map drops keys.** Tab, F1, Left, Right, Home, End, Delete, PgUp, PgDn and Shift+Tab never reach the app (bin:48-62). Suggested approach: extend `key_input` with the NSH names (ArrowLeft, ArrowRight, Home, End, Delete, Tab, F1, PageUp, PageDown) and add a Shift+Tab route for the approval-mode flip. Work: app only.
2. **Multi-line entry.** No newline key works (Shift+Enter, Ctrl+J, backslash), and bracketed paste is not enabled, so pasted multi-line text submits each line (inferred). Suggested approach: enable EnableBracketedPaste in terminal setup; switch the composer to NSH SubmitBehavior::Newline with Enter as the submit key; add the newline keys. Work: app only, to be verified in a real terminal.
3. **Esc on an approval interrupts the run** instead of denying the prompt (chat.rs:405 runs before the approval block). Suggested approach: when an approval is pending, map Esc to Decide(deny) before the Interrupt arm. Work: app only.
4. **No input history.** Up and Down do not recall sent inputs. Suggested approach: keep sent inputs in app state and route Up/Down to recall when the input is one line. Work: app only.
5. **No @file or !command.** No file completion, and no shell passthrough with its output. Suggested approach: a workspace walk for @ completion, and a `!` path that runs on the machine where the tools run (the client in remote mode, as src/run.rs:886-894 does). Work: app only.
6. **No credential path.** /auth opens Settings, which has no key entry, and there is no onboarding for a first run. Suggested approach: a credentials screen over the existing core calls auth_providers, auth_start and auth_set_key (core/client.rs:838-870); decide how first-run detection works. Work: app only, if the server answers AuthProvidersReq as the protocol defines (inferred, not checked).
7. **No tool detail panel** (Ctrl+T and click). The snapshot has no per-event list, although core/view.rs:543 holds one on the view model. Suggested approach: add the list to the snapshot and render a panel. Work: core, then app.
8. **Scrollback has no keys or wheel.** PgUp and PgDn are unmapped (item 1), and mouse capture is off, so there is no wheel scroll and no click or drag selection. Suggested approach: keyboard paging first; decide whether mouse capture is wanted, since it changes native selection. Work: app only.
9. **Session id is never shown,** and nothing prints it on exit (src/run.rs:400-405). Suggested approach: show the id in the status line and print the resume command on exit. Work: app only.
10. **No image attachments.** Ctrl+V does nothing, and submit sends empty attachments (core/client.rs:529-545). Suggested approach: pass attachments through core submit, then add the chip row and the clipboard read. Work: core, then app.
11. **Startup flags are not forwarded.** --yolo, --goal, --sandbox, --effort, --agent, --allow, --deny, --working-dir, --sessions-dir and --resume are rejected or dropped (startup.rs:56-92, 320-325). Suggested approach: parse and forward them to the child `modron serve`. Work: app only (inferred: serve takes the global flags).
12. **Task-submitting commands missing.** /review, /btw, /learn, and /plan with a task (which now drops the task). /goal can use the existing ConfigPatch.goal. Suggested approach: submit canned tasks as the old TUI did, and stop dropping /plan arguments. Work: app only; /goal also app only.
13. **/model picker** lacks the current marker, preselect and no-key marking, and Shift+S save-default is not wired though core/client.rs:949 persist_router exists. Work: app only.
14. **Reasoning display** (/reasoning and the live tail) is unwired although the core carries show_reasoning and streaming_reasoning. Work: app only.
15. **Ctrl+C exits at once** mid-turn; the old TUI aborted first (src/lib.rs:949-956). Suggested approach: abort on the first Ctrl+C while a turn runs. Work: app only.
16. **No unix-socket transport and no LAN opt-in.** The old default was the unix socket (src/remote.rs:88-91); plain ws:// to a LAN host needs ConnectOptions.allow_insecure (core/client.rs:77-89). Work: core for the socket transport; app for the LAN flag. Needed only for users who rely on those paths.
17. **Device token from ?token= or MODRON_DEVICE_TOKEN** is not read in the terminal host (src/remote.rs:36-80). Work: app only.
18. **Surfaces with core support but no screen:** /automations, /worktree, /agent, /allow and /deny, and the auth manager. Work: app only.
19. **Low priority:** /exit (and /quit, /q), /clear, the aliases listed in the slash table, the /context breakdown, the "bubblewrap" spelling for /sandbox, help tabs and scroll, toast expiry, the "esc to interrupt" copy, the idle hints, and the welcome banner. Work: app only.
20. **Decision needed:** theme switching and custom theme files (see the next section).

## Deliberately not carried over

- Ctrl+O transcript-detail toggle (src/lib.rs:925-928). It changed only the status label (src/lib.rs:3091), and nothing else read transcript_mode. Ctrl+O now opens Sessions.
- Old meaning of Alt+Enter as newline. The app's help assigns Alt+Enter to queue (app/src/help.rs:54-62). This is a deliberate reassignment, but it leaves newline entry dependent on the Shift+Enter and Ctrl+J gap (item 2).
- Enter refused while a turn runs (src/run.rs:913-916). The app steers the running turn instead (chat.rs and session.rs apply Send). A deliberate change.
- /share publishing the last answer (src/lib.rs:2639-2648). The shared table defines /share as publishing the transcript (view slash.rs:103-108).
- /plan <task> submitting a plan-first task (src/lib.rs:2655-2664). The shared table defines /plan as a phase switch (view slash.rs:91-96). The dropped argument is still a bug (item 12).
- The engine running in-process with no server (src/run.rs:443-474). By design the app is always a client of modron serve (app/README.md).
- `modron rewind` and the rewind-marker resume. These are CLI features, not TUI ones.
- tui-dev hot reload. No such code exists in the repository.
- The update notice. The old TUI never drew it.
- The context-meter bar (src/lib.rs:3863-3875). Only tests call it.
- Candidate, needs sign-off, not decided here: theme switching and custom theme files (src/theme.rs:44-124, src/lib.rs:889-897). The shared divergence table gives theming to each client (its Theming row); the app ships one oculus theme. Keep or drop is a product decision.

## In the new app but not the old TUI

- Sessions screen: Ctrl+O or /sessions, filter, n for a new session, d to delete with an inline confirm, j/k, Enter to attach (app/src/chat.rs route_screen_key; app/src/nav.rs; app/src/screens.rs sessions_body, 160).
- Settings screen rows: model, approval mode, phase, effort, sandbox and compact act; provider, context and rules are read-only (app/src/nav.rs settings_rows, 641; F2 or /settings).
- Plan decision block: y approves, r revises with the composer text as the note (app/src/chat.rs plan_block, 1119; route_key PLAN_KEYS).
- Phase and effort commands and status: /plan and /build switch the phase, /effort sets reasoning effort; the status shows phase in plan and effort when set (app/src/slash.rs run, 138-141; core/view.rs:776-790; shared table view slash.rs:86-102).
- Queue: Alt+Enter queues; Backspace on an empty composer removes the last queued message; queue chips show above the composer (app/src/chat.rs queue_chips, 1033).
- Steering: Enter while a turn runs steers it; steer echoes show above the composer (app/src/chat.rs steer rows; core/client.rs:605-630).
- Sub-agent screen: Ctrl+A opens the first running agent; steer by Enter; kill with a y/n confirm (app/src/nav.rs OpenAgent; app/src/screens.rs agent_panel, 346).
- Command list: Esc hides it, Tab and Enter complete the selected command (app/src/chat.rs route_key).
- Reconnect with backoff (2 s doubling to 30 s) and re-attach to the same session; Esc or Ctrl+C leaves while retrying (app/src/session.rs retry_delay, 269; step, 341; app/tests/terminal.rs dropped_connection_reattaches_to_the_same_session).
- Connection header with state, provider and address; "started by modron-app" when the app started the server (app/src/chat.rs header, 964; core/view.rs OWN_SERVER_NOTE).
- Owned server: the app starts `modron serve` as a child on loopback, stops it on exit, and keeps it with --keep-server (app/src/startup.rs:303-364).
- Startup source order: MODRON_URL probe, then start a server, then a fatal screen with the two ways out (app/src/startup.rs decide, 134; app/README.md).
- Flags --url, --token, --session, --new, --continue, --provider, --model, --demo, --keep-server (app/src/startup.rs:61-70).
- Built-in demo: --demo plays a scripted session (app/src/demo.rs).
- Model picker from the server catalog with availability; unavailable models cannot be selected (app/src/nav.rs model_rows, 602).
- Context gauge from the server, replacing the old segment that never received data (core/view.rs:90-103, 790-795).
- Confirm prompts for session delete and agent kill (app/src/nav.rs AskDelete, AskKill).
- Share links: a `session` attach parameter adopts the session the snapshot names (app/NOTES.md, Packet C3e; app/tests/live.rs).
- Web target controls: Stop, Queue and chip Dequeue buttons (app/src/chat.rs WEB_CONTROLS, 146). Not part of the terminal.
- Owner portal (web only, uncommitted): sign-in, session list with refresh, invites (app/src/portal.rs; app/src/screens.rs portal_frame; app/src/nav.rs Portal and SignIn). Not reachable from the terminal host.
- F2 (settings) and F1 (help) are new keys. F1 cannot be pressed in the terminal host because it is not mapped (bin:48-62), although the status hint advertises it (screens.rs:35).

## Not verified

- No cargo run, no terminal session, no web run. Statements about key handling come from reading `key_input` and the routing code; the NSH behaviours come from the NSH 0.5.1 sources in the local cargo registry.
- Items marked (inferred) were not run.
- Startup time was not measured.
- The uncommitted app edits may change the statuses above when they land.
