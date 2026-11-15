# Modron app parity audit (old terminal UI retirement)

Packet PARITY-2. Read-only audit, rewritten 2026-11-15. No cargo run, no commit. Supersedes the audit written at 3afd1f5.

## Scope and state

- Old TUI: `crates/modron-tui/` (all `src/...` citations below are relative to it). Frozen. Sampled citations were re-checked; the slash handler arms in `src/lib.rs` (2497-2716) were 10-16 lines off in the previous audit and are corrected here. The parse table is `src/lib.rs` 240-269.
- New app: `app/` (`app/src/...`, `app/tests/...`, `app/web-test.js`, `app/README.md`, `app/NOTES.md`). Client core: `mobile/rust-core/src/{client.rs,view.rs,attach.rs}`. Shared table: `crates/modron-view/src/slash.rs`. Protocol: `crates/modron-protocol/src/lib.rs`. Server and engine are cited where a row depends on them (`crates/modron-server/src/dispatch.rs`, `crates/modron-core/src/engine.rs`).
- Baseline: HEAD `22dad37` ("App: private shell commands, save default model, image paste, shared slash merge"). Line numbers for committed code refer to HEAD.
- Working tree: read twice. First read: uncommitted edits in `app/src/chat.rs`, `help.rs`, `nav.rs`, `screens.rs`, `session.rs`, `slash.rs` and `mobile/rust-core/src/view.rs` by another agent, adding an automations screen and a tasks screen (routes, inline forms, row labels, new `UiAction` variants, and the `/tasks`, `/worktree`, `/automations` arms). Second read: also `app/src/clipboard.rs` (macOS image paste through `osascript` JXA), `/clear` (a `slash.rs` arm and `UiAction::Clear`, with `chat.rs` `clear_view`), and note expiry (`NOTE_TTL`, `note_expired`, `shown_copy`, `expire_note` in uncommitted `chat.rs`). Rows that depend on that uncommitted code are marked "(in progress)" and cite it. Not started: the web attach control (no diff line mentions an attach control; only task attach).
- Not run: cargo, the Rust test suites, `app/web-test.js`, the e2e harness. Test names are cited from source. A cited test exists; it was not run.
- Statuses: DONE (app file:line and a test, `file::test_name`; "untested" when no test was found), PARTIAL (present, with the gap named), CORE-ONLY (core supports it, no app screen or key does), MISSING, NOT-APPLICABLE (reason given), DIFFERENT-BY-DECISION (PLAN 10.2a or a ledger row cited, both behaviours stated).
- Rows: the same 140 rows in the same order as the previous audit, so the counts compare directly.

## Summary

Counts: DONE 88 (was 52), PARTIAL 27 (was 17), CORE-ONLY 1 (was 15), MISSING 16 (was 50), NOT-APPLICABLE 7 (was 6; Ctrl+O moved in from MISSING), DIFFERENT-BY-DECISION 1 (was 0; `!command`). Total 140. Nine rows are marked "(in progress)": they depend on the uncommitted automations and tasks screens, `/clear`, note expiry or the macOS clipboard image.

Change since the previous audit (3afd1f5 to 22dad37): the terminal key map now forwards every named key (Tab, F1, arrows, Home, End, Delete, PgUp, PgDn, Shift+Tab) and turns on bracketed paste, so multi-line entry, Shift+Enter, Ctrl+J, backslash continuation, input history, PgUp/PgDn, Ctrl+T, `@file` and paste work in the terminal. Esc denies a pending approval. `!command` and `!!command` exist (PLAN 10.2a). Shift+S saves the default model. The credentials screen, `/review`, `/btw`, `/learn`, `/plan <task>`, `/goal`, `/agent`, `/allow`, `/deny`, `/exit`, and the shared aliases are wired. Startup flags are forwarded, and unix sockets, `?token=`, `MODRON_DEVICE_TOKEN` and `--insecure-lan` are supported. Linux clipboard image paste exists. The previous audit's CORE-ONLY rows mostly became DONE or PARTIAL because the app now has screens or keys for them.

Still open, by count: 3 slash commands are missing (`/context`, `/issues`, `/theme`); `/clear`, `/automations`, `/tasks` and `/worktree` are written in the uncommitted tree; the pickers for mode, sandbox, persona, allow/deny and theme are missing; the terminal has no mouse (wheel, click, drag-to-copy); `/goal` is set but not shown; the 80 percent context warning, spinner, turn count, esc-to-interrupt hint and idle hints are missing; the welcome banner and help tabs are missing.

## Keys

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Enter sends the task | src/lib.rs:1116-1157 | DONE | `chat.rs` `route_key` leaves Enter to the composer; the composer submit sends the task (`session.rs` apply Send, `client.rs:736` send_message). Test: `app/tests/chat.rs::typing_and_enter_submits_and_keeps_focus`. |
| Enter while a turn runs | src/run.rs:913-916 (refused with a toast) | DONE | Steers the running turn: `client.rs:768` `steer`, chosen by `send_message` while running. Differs from the old refusal (see "Deliberately not carried over"). Test: `app/tests/live.rs::driver_renders_the_stream_and_steers_a_typed_message`. |
| Shift+Enter or Alt+Enter inserts a newline | src/lib.rs:1103-1115 | PARTIAL | Shift+Enter inserts a line (`app/src/bin/modron-app-tui.rs` `key_input`, `is_newline_chord`). Alt+Enter queues instead (`chat.rs` `queue_route`; `app/src/help.rs` lines 54-62). Test: `app/tests/chat.rs::shift_enter_adds_a_line_and_enter_submits_the_whole_draft`. |
| Ctrl+J inserts a newline | src/lib.rs:942-945 | DONE | `bin` `is_newline_chord` maps Ctrl+J to Shift+Enter. Test: `bin::ctrl_j_and_ctrl_enter_are_shift_enter`. |
| Backslash then Enter continues the line | src/lib.rs:1117-1125 | DONE | Test: `app/tests/chat.rs::a_trailing_backslash_continues_the_line_and_does_not_submit`. |
| Up/Down recall sent inputs (single line) | src/lib.rs:1206-1213, 1935-1959 | DONE | `chat.rs` `route_key` recall when the draft is empty or one line; `Host::recall` (`chat.rs`). Test: `app/tests/chat.rs::up_recalls_the_last_sent_input_and_down_restores_the_draft`. |
| Up/Down move between lines of a multi-line input | src/lib.rs:1198-1205, 1963-1977 | DONE | A multi-line draft is not recalled, so Up/Down reach the NSH composer. Test: `chat.rs::up_and_down_recall_only_on_an_empty_or_recalled_single_line` covers the routing; caret movement inside NSH is untested. |
| Left / Right move the cursor | src/lib.rs:1179-1186 | DONE | `bin` `key_name` maps ArrowLeft and ArrowRight; `route_key` sends them to the composer. Test: `bin::key_map_forwards_the_keys_the_app_uses` (routing only; caret editing is NSH's and untested). |
| Home / End | src/lib.rs:1187-1194 | DONE | `route_key` scrolls only on an empty draft, otherwise moves the caret. Test: `chat.rs::page_keys_scroll_and_home_end_scroll_only_on_an_empty_composer`. |
| Delete | src/lib.rs:1169-1178 | DONE | `bin` maps Delete; `route_key` sends it to the composer. Test: `bin::key_map_forwards_the_keys_the_app_uses` (key name only; editing is NSH's and untested). |
| Backspace edits; on an empty composer it removes the last queued message | src/lib.rs:1158-1168 | DONE | `route_key` Backspace dequeues when the draft is empty and messages are queued. Tests: `app/tests/live.rs::backspace_on_an_empty_composer_dequeues_the_last_message`; `chat.rs::backspace_on_an_empty_composer_dequeues_the_last_message`. The ledger calls the live test racy under load. |
| Esc interrupts a running turn | src/lib.rs:1048-1050 | DONE | `chat.rs` `escape_route` returns Interrupt when running; `client.rs:676` interrupt. Test: `app/tests/chat.rs::escape_sends_interrupt`. |
| Esc when idle: clear selection, leave scrollback, clear input | src/lib.rs:1240-1251 | MISSING | `escape_route` returns Ignored when idle (`chat.rs`). Nothing clears the composer or resets the scroll follow. No test. |
| Esc closes the open screen | src/lib.rs:1006-1011, 1029-1036 | DONE | `route_screen_key` for Help, Agent, Models, Auth, Settings and Sessions. Tests: `chat.rs::esc_closes_settings_and_interrupts_only_on_chat`; `app/tests/nav.rs::esc_closes_settings`; `app/tests/c3f.rs::esc_clears_the_model_filter_before_closing_the_picker`. |
| Esc on a pending approval denies it | src/lib.rs:984-986 | DONE | `escape_route` checks the approval before the interrupt and denies with the last option. Fixed since the previous audit. Tests: `chat.rs::escape_with_an_approval_denies_it_before_anything_else`; `app/tests/chat.rs::esc_denies_a_pending_approval_instead_of_interrupting`. |
| Ctrl+C aborts a running turn; quits when idle | src/lib.rs:949-956 | DONE | `bin` `ctrl_c`: the first press during a turn interrupts; a second within 2 s, or a press when idle, exits. Test: `bin::ctrl_c_interrupts_once_mid_turn_and_exits_otherwise`. |
| Ctrl+O toggles transcript detail (compact or full) | src/lib.rs:925-928, 3089-3098 | NOT-APPLICABLE | The old toggle changed only a status label (src/lib.rs:3091); nothing else read it. Ctrl+O opens Sessions (`chat.rs` route_key chord arm). See "Deliberately not carried over". |
| Ctrl+T expands this turn's tool activity | src/lib.rs:930-938, 3382-3409 | DONE | `route_key` chord t toggles the panel; `route_tool_panel_key` handles it. Tests: `app/tests/d1.rs::ctrl_t_opens_the_tool_panel_with_the_snapshot_rows_and_esc_closes_it`; web: `app/web-test.js` `testCtrlTAndShellPassthrough`. |
| Ctrl+V pastes an image as an attachment, or text into the input | src/lib.rs:946-948; src/clipboard.rs:55-64 | PARTIAL (in progress) | Images: `app/src/session.rs` `paste_clipboard` stages an image on Linux (`app/src/clipboard.rs` `harvest_with`, wl-paste then xclip). Text is not inserted: the app says "clipboard has text, not an image: paste it with the terminal" (`session.rs:832`). At HEAD macOS images are skipped (`clipboard.rs:107`). The uncommitted `clipboard.rs` reads macOS images through `osascript` JXA (`MACOS_IMAGE_JXA`, `harvest_with_os`); untested. Tests at HEAD: `app/tests/app3.rs::ctrl_v_asks_for_a_paste_and_a_staged_image_shows_its_chip`; `chat.rs::ctrl_v_pastes_and_backspace_drops_a_chip_before_a_queued_message`; `clipboard.rs` tests (`an_image_on_the_clipboard_is_harvested_before_text`). |
| Pasted text enters the input verbatim, including multi-line | src/run.rs:807-815 (bracketed paste enabled at src/run.rs:173-176) | DONE | `bin` enables `EnableBracketedPaste` and forwards `Event::Paste` as `Input::Paste`. Test: `app/tests/chat.rs::a_pasted_block_lands_whole_and_nothing_submits`. |
| Shift+Tab flips the approval mode | src/lib.rs:1229-1239 | DONE | `bin` `key_name` maps BackTab; `route_key` BackTab sends `CycleMode`. Tests: `chat.rs::shift_tab_flips_the_approval_mode_and_caret_keys_stay_in_the_composer`; `bin::key_map_forwards_the_keys_the_app_uses`. |
| PgUp / PgDn page the scrollback | src/lib.rs:1215-1224 | DONE | `route_key` PageUp and PageDown send `Scroll`. Tests: `chat.rs::page_keys_scroll_and_home_end_scroll_only_on_an_empty_composer`; `app/tests/chat.rs::paging_keys_scroll_without_touching_the_composer`. |
| Mouse wheel scrolls the scrollback | src/run.rs:837-838 | MISSING | The terminal host never enables mouse capture and maps no mouse events (`bin` has no EnableMouseCapture). The web target handles the wheel (`app/src/web.rs` 377-390; `web-test.js` `testWheelScrollsTranscript`). Terminal only. |
| Click positions the input cursor | src/lib.rs:1997-2043 | MISSING | Same as the mouse wheel row. |
| Drag selects scrollback text; release copies it | src/run.rs:816-835; src/lib.rs:2067-2149; src/clipboard.rs:27-52 | PARTIAL | No in-app selection and no copy-on-release. Native terminal selection should still work because mouse capture is off (inferred, not run). |
| Click a tool summary or the tool strip opens the panel; click outside closes it | src/lib.rs:1997-2013, 2067-2072 | PARTIAL | Web only: `chat.rs` `tool_strip` toggles the panel on press (under `WEB_CONTROLS`, `chat.rs` near line 1668). Terminal: no mouse events. |
| Tab completes the highlighted /command | src/lib.rs:1068-1072 | DONE | `bin` `key_name` maps Tab; `route_key` Tab when suggesting sends `CompleteSelected`. Test: `app/src/help.rs::every_binding_routes_as_the_table_says` (the Tab binding, `help.rs:100`). |
| Up/Down move the highlight in the command list | src/lib.rs:1059-1066 | DONE | `route_key` Up/Down when suggesting sends `SlashMove`. Test: `help.rs::every_binding_routes_as_the_table_says` (ArrowUp and ArrowDown bindings, `help.rs:165`, `:240`). |
| Tab completes an @path | src/lib.rs:1094-1097 | DONE | `route_key` Tab with `file_mode` sends `CompleteFile`; `app/src/files.rs` walks the workspace. Test: `app/tests/d1.rs::at_completion_lists_workspace_files_and_tab_splices_the_path`. |
| ? on an empty input opens help | src/lib.rs:1148-1150 | DONE | `route_key` Char('?') with an empty draft opens Help. Test: `app/tests/c3f.rs::f1_and_a_question_mark_open_help_and_esc_closes_it`. |
| y / a / n decide the approval; a allows all for the run | src/lib.rs:974-987 | DONE | `route_key` approval block (`DECISION_KEYS`); `approval_strip` (`chat.rs`). Tests: `chat.rs::approval_keys_decide_their_options_in_order`; `app/tests/chat.rs::a_during_pending_approval_decides_option_one`. |
| Approval type-ahead guard: decision keys typed as the prompt appears are ignored | src/lib.rs:962-973 | MISSING | No guard: `route_key` decides on the first decision key while an approval is pending. No test. |
| Shift+S on a model row saves it as the default | src/lib.rs:1553-1573; src/run.rs:1593-1601 | DONE | `route_screen_key` Models Char('S') sends the save action (`facts.save_default`, `chat.rs`); the save writes the default model and provider through `client.rs:1196` `persist_default_model`. Matches PLAN 10.2a. Tests: `app/tests/app3.rs::shift_s_saves_the_highlighted_model_and_provider_as_the_default`; `shift_s_on_an_unavailable_model_saves_nothing`. Web control: `app/src/screens.rs` 710-716. |

## Slash commands

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| /help and bare ? | src/lib.rs:2497-2504, 240 | DONE | `slash.rs` run "help" opens Help; `?` resolves through the shared table alias (`modron-view/src/slash.rs`). Tests: `app/tests/c3f.rs::f1_and_a_question_mark_open_help_and_esc_closes_it`; `slash.rs::every_table_command_is_mapped`. |
| /exit, /quit, /q | src/lib.rs:2496, 241 | DONE | `slash.rs` run maps them to `UiAction::Exit`; `chat.rs` sets the quit flag; `bin` run loop exits. Test: `slash.rs::local_commands_map_to_their_actions` (the mapping; the exit path itself is untested). |
| /clear (empties the scrollback) | src/lib.rs:2505-2516, 242 | DONE (in progress) | HEAD: no arm; falls to the unknown-command notice, and the shared table omits `clear` (`modron-view/src/slash.rs` test `every_tui_catalog_command_is_shared_or_tui_only` lists it as TUI-only). Working tree (uncommitted): `slash.rs` `"clear" => UiAction::Clear` with a mapping assert; `chat.rs` `clear_view` and `shown_copy` hide the rows on screen (nothing is sent). Untested.
| /compact | src/lib.rs:2517-2524, 243 | DONE | `slash.rs` "compact" -> `UiAction::Compact` -> `client.rs:881` compact. Test: `app/tests/live.rs::slash_compact_sends_compact_instead_of_text`. |
| /model [name] | src/lib.rs:2525-2545, 244 | DONE | Bare opens the picker; with a name it selects (`slash.rs` run). Picker: current marker and preselect (`nav.rs` `model_rows`, `screens.rs` `model_row`), dimmed unavailable rows. Tests: `app/src/nav.rs::the_current_model_is_marked_and_preselected_when_the_picker_opens`; `app/tests/c3f.rs::model_picker_filters_and_enter_switches_the_model`; `slash.rs::model_bare_opens_the_picker_and_with_a_name_switches`. |
| /context | src/lib.rs:2546-2552, 245 | MISSING | No arm: /context falls to the unknown-command notice. The status line shows the context percentage (see the status surface table), not the old breakdown. The shared table omits `context` (TUI-only by test). |
| /sessions | src/lib.rs:2553-2556, 246 | DONE | `slash.rs` "sessions" opens Sessions (`nav.rs` ListSessions). Test: `app/tests/nav.rs::opening_sessions_shows_the_core_rows_and_refreshes_the_list`. |
| /sandbox [off, seatbelt, bwrap] | src/lib.rs:2557-2566, 247 | PARTIAL | Bare /sandbox cycles (no picker). `bubblewrap` is rejected at the command (`slash.rs` run accepts only `bwrap`); the old parser accepted it. The `--sandbox` flag accepts both (`startup.rs` `sandbox_word`). No command-level test. |
| /reasoning (and /thinking) | src/lib.rs:2567-2575, 248 | DONE | `slash.rs` maps both to `ToggleReasoning` (`session.rs` -> `client.rs:610` set_show_reasoning); the live tail renders in `chat.rs` `screen` when `show_reasoning` is on. Mapping test: `slash.rs::every_table_command_is_mapped`. Toggle and tail untested. |
| /btw <question> | src/lib.rs:2576-2585, 249 | DONE | `slash.rs` "btw" -> `Send(btw_task(args))`, which appends the shared suffix (`modron-view/src/slash.rs:286`); bare shows the usage. Tests: `slash.rs::local_commands_map_to_their_actions` (`/btw why`); `modron-view/src/slash.rs::canned_prompts_match_the_old_tui_text`. |
| /agent [name] (and /persona) | src/lib.rs:2599-2606, 250 | PARTIAL | With a name: `SetPersona` (`session.rs` -> `client.rs:936` set_persona). Bare shows the usage line. The old persona picker (src/lib.rs:1763-1784) is missing. Test: `slash.rs::local_commands_map_to_their_actions` (`/agent reviewer`). |
| /allow [tool], /deny [tool] | src/lib.rs:2607-2630, 251-252 | PARTIAL | With a tool: `AddToolRule` (`session.rs:638` -> `client.rs:945` set_tool_lists). Bare prints the lists as a note (`session.rs:650` ShowToolLists). No removal and no picker. Test: mapping only (`slash.rs::every_table_command_is_mapped`). |
| /mode [ask, yolo, deny] | src/lib.rs:2623-2630, 253 | PARTIAL | With a mode: sets it. Bare `/mode` cycles (`CycleMode`) instead of opening the picker. Test: `slash.rs::local_commands_map_to_their_actions` (`/mode yolo`, `/mode`). |
| /tasks | src/lib.rs:2631-2634, 254 | PARTIAL (in progress) | HEAD: sends a status request (`slash.rs` "tasks" -> `TaskStatus` -> `client.rs:1136` task_status), DONE in that sense. Working tree (uncommitted): `/tasks` opens a Tasks screen (`slash.rs` "tasks" -> `Nav(Open(Tasks))`; `nav.rs`, `screens.rs` `tasks_body`, `chat.rs` `route_list_screen_key`). Untested. |
| /worktree (and /worktrees) | src/lib.rs:2635-2638, 255; src/run.rs:1698-1714 | PARTIAL (in progress) | HEAD: `/worktree <title>` creates a task (`CreateTask` -> `client.rs:1123` create_task); bare shows usage. Working tree: bare `/worktree` opens the Tasks screen (uncommitted `slash.rs`). Merge exists in core (`client.rs:1145`) and is wired only in the uncommitted screen. |
| /issues [add, done] | src/lib.rs:2639-2650, 256; src/run.rs:1868-1936 | MISSING | No arm; the core has no issues call and the protocol has no issues frame (`crates/modron-protocol/src/lib.rs`). The old command reads `<memory_root>/<slug>/issues.jsonl` on the server (src/run.rs:1868). Needs a server frame. |
| /fork | src/lib.rs:2643-2646, 257 | DONE | `slash.rs` -> `UiAction::Fork` (`session.rs:660`) -> `client.rs:1113` fork_session. Untested at app level. |
| /plan <task> (plan-first task) | src/lib.rs:2671-2686, 263 | DONE | `slash.rs` "plan" with a task -> `Plan(plan_task(args))`: sets the plan phase (`client.rs:900`) and sends the task (`session.rs:677`). Bare `/plan` sets the phase only. Test: `slash.rs::local_commands_map_to_their_actions` (`/plan add tests`). Previously the task was dropped. |
| /review (and /code-review) | src/lib.rs:212 (catalog; handler not re-located), 2665-2670 (not re-verified) | DONE | `slash.rs` "review" | "code-review" -> `Send(REVIEW_TASK)`. Tests: `slash.rs::local_commands_map_to_their_actions` (`/review`); `modron-view/src/slash.rs::canned_prompts_match_the_old_tui_text`. |
| /goal [condition] | src/lib.rs:2687-2701, 265 | PARTIAL | Sets the goal: `slash.rs` -> `SetGoal` -> `session.rs:681` -> `client.rs:927` set_goal. Not shown: the status line has no goal (`mobile/rust-core/src/view.rs` status line), though the snapshot carries it (`view.rs` goal field). Tests: `slash.rs::local_commands_map_to_their_actions` (`/goal ship it`); `app/tests/d1.rs` startup config forwards goal. |
| /learn [title] | src/lib.rs:2702-2715, 266 | DONE | `slash.rs` "learn" -> `Send(learn_task(args))`. Tests: `slash.rs::local_commands_map_to_their_actions` (`/learn Fix Parser`); `modron-view` canned-prompt test. |
| /theme [name] | src/lib.rs:2716-2722, 267; src/theme.rs:44-124 | MISSING | No arm (unknown notice). The shared table omits `theme` (TUI-only). `app/src/lib.rs` `install_theme` always installs oculus. Decision needed. |
| /auth [args] | src/lib.rs:2586-2598, 268 | PARTIAL | `/auth` and `/login` open the Auth screen; the arguments are dropped (`slash.rs` run). The screen has provider rows (`nav.rs` `auth_rows`), OAuth start, masked key entry, and the sign-in wait copy (`screens.rs:201`). No remove: the core has no auth-remove call (`client.rs` 1064-1101 has providers, start and set_key only). Tests: `nav.rs::opening_the_screen_asks_for_the_provider_rows_and_settings_opens_it`; `nav.rs::enter_on_a_key_row_opens_the_entry_and_the_submit_or_escape_closes_it`; `chat.rs::the_secret_key_never_prints`. |
| /settings | src/lib.rs:262 (parsed as Auth in the old TUI) | PARTIAL | `slash.rs` "settings" -> Settings rows: model, approval, phase, effort, sandbox and compact act; provider, context and rules are read-only (`nav.rs` `settings_rows`). The old `/settings` opened the auth manager; the app's is `/auth`. Test: `app/tests/nav.rs::settings_shows_the_core_labels_and_enter_on_approval_mode_sends_the_mode`. |
| /new | src/lib.rs:2667-2670, 261; src/run.rs:1490-1496 | DONE | `slash.rs` "new" -> `NewSession` -> `client.rs:1101` new_session. Test: `app/tests/nav.rs::n_creates_a_session_and_returns_to_chat`; `app/src/nav.rs::new_session_goes_to_chat`. |
| /share | src/lib.rs:2655-2666, 260; src/run.rs:1064-1070 | DONE | `slash.rs` "share" | "publish" -> `Share` (`session.rs:662`) -> `client.rs:1023` publish_page with the whole transcript. The old command published the last answer. Untested at app level. |
| /remote-control [url] | src/lib.rs:2631-2634 (old citation; not re-verified), src/run.rs:1049-1057 | DONE | `slash.rs` "remote-control" | "rc" -> `RemoteControl` (`session.rs:672`) -> `client.rs:1041` remote_control. Mapping test: `slash.rs::local_commands_map_to_their_actions`. Call untested at app level. |
| /automations | src/lib.rs:2651-2654, 259 | PARTIAL (in progress) | HEAD: prints "automations screen is not in this app yet" (`slash.rs`). Working tree: `/automations` opens an Automations screen with a list, an inline add form and a remove confirm (`nav.rs`, `screens.rs` `automations_body`, `chat.rs` `route_list_screen_key`; labels in `mobile/rust-core/src/view.rs`). Core list, add and remove exist (`client.rs:987-1014`). Untested. |
| Unknown command | src/lib.rs:2726-2729 | DONE | `slash.rs` run's fallback returns `SLASH_UNKNOWN_LABEL`. Test: `slash.rs::unknown_words_get_the_shared_notice`. |
| Aliases: /? /compress /login /publish /rc /thinking /persona /worktrees /code-review | src/lib.rs:185-227, 240-269 | DONE | Shared aliases in `modron-view/src/slash.rs` (`SlashSpec::aliases`; `?` on help, `q` and `quit` on exit); `slash.rs` run resolves them and has the app arms (`compress`, `login`, `publish`, `rc`, `thinking`, `persona`, `worktrees`, `code-review`). Tests: `modron-view/src/slash.rs::every_alias_resolves_to_its_command`; `aliases_match_the_old_tui_alias_rows`; `slash.rs::local_commands_map_to_their_actions` (`/login`, `/rc`, `/q`). |

## Input prefixes

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| !command shell passthrough (runs in the workspace, or on the client when remote) | src/run.rs:871-908, 1839-1841 | DIFFERENT-BY-DECISION | See "Different by decision". The app runs `!cmd` through the core shell call (`client.rs:815` shell), so the server runs it through `engine.rs:2283` `run_user_shell`, with the deny list, rules, sandbox and hooks. Tests: `app/tests/d1.rs::a_bang_line_is_a_shell_action_and_never_a_submit`; `app/src/chat.rs::bang_is_shared_and_double_bang_is_private`. |
| !command output: stdout, stderr, exit code; capped at 10 KiB | src/run.rs:1307-1327, 1839-1857 (cap_output at 1847-1857) | DONE | Cap in the engine: `engine.rs:124` `USER_SHELL_MAX_BYTES` = 10 KiB. The app renders the command header and dimmed output rows (`chat.rs` shell rows). Tests: `app/src/chat.rs::shell_rows_put_the_command_first_and_dim_the_output`; `app/tests/shell.rs::shell_rows_render_the_command_header_and_their_output`. The cap itself has no app test. |
| @path mention: popup, Up/Down, Tab completes | src/lib.rs:1077-1100, 2170-2248, 3706-3730 | DONE | `app/src/suggest.rs` (`for_draft`, `splice_file`), `app/src/files.rs` (gitignore-aware walk with a cap), `chat.rs` `file_mode`. Tests: `app/tests/d1.rs::at_completion_lists_workspace_files_and_tab_splices_the_path`; `suggest.rs::at_must_start_a_token`; `files.rs::walk_respects_gitignore_and_skips_the_heavy_dirs_and_hidden_entries`. The web keeps `@` as text. |
| Image attachments (Ctrl+V) and attachment chips | src/lib.rs:2984-3018, 3778-3787 | PARTIAL (in progress) | Linux paste stages a chip (see Ctrl+V). Chips render above the composer (`chat.rs` `attachment_rows`); Backspace drops the last chip first. Submit sends the staged images (`client.rs:659` submit_with). Attachments are refused while the session runs (`client.rs` around 750-760). Missing: a tested macOS image path (written, uncommitted, in `clipboard.rs`) and a web attach control (no code). Tests: `app/tests/app3.rs::ctrl_v_asks_for_a_paste_and_a_staged_image_shows_its_chip`; `chat.rs::chips_show_name_and_the_shared_size`. |
| Dropped file paths arrive as text | src/run.rs:807-815 | DONE | A dropped path arrives as bracketed paste text (`bin` Event::Paste) and attaches nothing. Test: `app/tests/chat.rs::a_pasted_block_lands_whole_and_nothing_submits`. |

## Screens and popups

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Scrollback of committed transcript, wrapped | src/lib.rs:2265-2296, 3308-3325 | DONE | `chat.rs` `transcript_lines` and `style_of` over core history rows (`mobile/rust-core/src/view.rs`). Test: `app/tests/chat.rs::transcript_rows_render_in_order`. |
| Scrollback paging and the "lines above" indicator | src/lib.rs:1215-1224, 3309-3316 | PARTIAL | Paging works (Keys). The "new output below" indicator exists (`chat.rs` `NEW_OUTPUT_BELOW_LABEL`, shown while scrolled up). No "lines above" count. Test: `app/tests/chat.rs::paging_keys_scroll_without_touching_the_composer`. |
| Live answer tail while streaming | src/lib.rs:3288-3295, 2734-2745 | DONE | `chat.rs` `screen` draws the core streaming text. Test: `app/tests/chat.rs::streaming_text_appears_and_clears`. |
| Live reasoning tail (opt-in) | src/lib.rs:3279-3287, 2737-2741 | DONE | `Model.reasoning` (`chat.rs` `from_snapshot`) fills only when `show_reasoning`; rendered in `screen` (`chat.rs` near line 1968). No test. |
| Live tool strip: turn summary and latest rows | src/lib.rs:3411-3429 | DONE | `chat.rs` `tool_strip` shows the core turn summary and the last rows. Test: `app/tests/chat.rs::live_tool_strip_swarm_rows_and_notes_render_verbatim`. |
| Expanded tool panel: all events, scroll, Esc closes | src/lib.rs:3382-3409, 2825-2830 | DONE | `chat.rs` `tool_panel` and `route_tool_panel_key`; the core snapshot carries `tool_detail`. Tests: `app/tests/d1.rs::ctrl_t_opens_the_tool_panel_with_the_snapshot_rows_and_esc_closes_it`; `d1.rs::the_panel_pages_and_jumps_with_the_transcript_keys`. |
| Sub-agent rows above the input | src/lib.rs:3329-3380, 2762-2793 | DONE | `chat.rs` `swarm_slots` over core `subagent_rows`. Tests: `app/tests/chat.rs::live_tool_strip_swarm_rows_and_notes_render_verbatim`; `app/tests/c3f.rs::the_agent_screen_steers_and_kills_with_confirmation`. |
| Approval strip: tool, args summary, three options | src/lib.rs:3431-3440 | DONE | `chat.rs` `approval_strip`. Test: `app/tests/chat.rs::pending_approval_shows_all_three_labels`. |
| Command list above the input: filtered, windowed | src/lib.rs:3669-3705 | DONE | `chat.rs` `slash_list` over `slash.rs` `suggestions_for`. Test: `app/tests/c3f.rs::typing_a_slash_shows_suggestions_and_esc_hides_them_keeping_the_text`. |
| Model picker: filter, current marker, provider and window detail, no-key marking | src/lib.rs:1643-1676, 3584-3630 | DONE | `nav.rs` `model_rows` marks the current model and preselects it; unavailable rows are dim (`screens.rs` `model_row`) and Enter ignores them. Provider and window detail come from the core label, shown verbatim. Tests: `app/src/nav.rs::the_current_model_is_marked_and_preselected_when_the_picker_opens`; `nav.rs::the_model_picker_lists_the_catalog_and_skips_unavailable_rows`; `nav.rs::the_rows_show_the_core_label_and_status_verbatim`; `app/tests/c3f.rs::model_picker_filters_and_enter_switches_the_model`. |
| Theme picker | src/lib.rs:1679-1707 | MISSING | See `/theme`. |
| Sandbox picker | src/lib.rs:1711-1737 | MISSING | Bare `/sandbox` cycles instead. |
| Mode picker | src/lib.rs:1740-1760 | MISSING | Bare `/mode` cycles instead. |
| Persona picker | src/lib.rs:1763-1784 | MISSING | No persona list in the snapshot (`mobile/rust-core/src/view.rs`). Setting works through `/agent`. |
| Allow and deny tool picker | src/lib.rs:1787-1820 | MISSING | No picker. `/allow` and `/deny` add rules; bare shows the lists as a note; no removal. |
| Auth manager: provider picker, method choice, masked key entry, browser or device sign-in wait, remove | src/lib.rs:416-496, 1264-1485, 3448-3581 | PARTIAL | See `/auth`. Provider rows, OAuth start, masked key entry and the sign-in wait copy exist. Remove is missing (no core call). Tests: `app/src/nav.rs::enter_on_an_oauth_row_starts_the_sign_in_and_keeps_no_entry`; `app/src/chat.rs::the_secret_key_never_prints`. |
| Help modal: three tabs (General, Commands, Shortcuts), scroll | src/lib.rs:275-412, 3635-3667 | PARTIAL | One page: `screens.rs` `help_body` lists the key and command headings, built from `help.rs`. No tabs, and Help takes only Esc (no scroll). Test: `app/src/help.rs::help_lists_every_binding_and_every_command`. |
| Toast strip: transient notes that expire after 40 ticks | src/lib.rs:905-908, 2410-2439, 3732-3745 | PARTIAL (in progress) | `chat.rs` `note_rows` shows the last note and last error (`NOTE_SLOTS = 2`). Nothing expires at HEAD. Uncommitted `chat.rs` adds `NOTE_TTL` (2 s, the old 40 ticks at 50 ms), `note_expired`, `shown_copy` (drops an expired note) and `expire_note`; untested. |
| Input box title "working (esc to interrupt)"; rotating placeholder hints | src/lib.rs:3747-3823 | PARTIAL | The composer uses the shared `COMPOSER_PLACEHOLDER` (`chat.rs` composer). The interrupt title and rotating hints are absent. No test. |
| Input box caret and scrolling input | src/lib.rs:1878-1930, 3844-3857 | PARTIAL | The composer is capped at 6 lines (`chat.rs` `COMPOSER_MAX_LINES`, line 325). Caret and scrolling are NSH's. Multi-line and arrow keys now reach the composer. No visual test. |
| Generative UI blocks rendered as aligned ASCII | src/lib.rs:2881-2898 | DONE | The core renders GenerativeUi with `modron_view::render_ascii` (`mobile/rust-core/src/view.rs`); the app shows the core lines. Untested in the app. |
| Turn-end usage line (tokens, turns, cache) | src/lib.rs:2907-2918 | DONE | Core Usage entry; `chat.rs` `style_of` gives it the meta style. Untested in the app. |
| Swarm summary at turn end | src/lib.rs:2762-2793 | DONE | Core Swarm entry through `transcript_lines`. Untested in the app. |
| Tool-activity summary line committed per turn | src/lib.rs:2799-2821 | DONE | Core ToolSummary entry. Test: `app/tests/chat.rs::turn_summary_and_tool_rows_leave_with_the_turn`. Click-to-expand is in the tool panel row. |
| Interrupted-turn note | src/lib.rs:2925-2940 | DONE | The core keeps the abort summary as the last note; the app shows it. Untested in the app. |
| Welcome banner | src/run.rs:541-575 | MISSING | No banner in `app/src/chat.rs` or `screens.rs`. |
| Update notice | src/lib.rs:588 (field only; view() never reads it) | NOT-APPLICABLE | The old TUI never draws the notice, so there is nothing to match. The CLI check remains (cli/main.rs:2915). |
| Context-meter bar | src/lib.rs:3863-3875 | NOT-APPLICABLE | Only tests call `context_meter` (crates/modron-tui/tests/state.rs:399-404). |

## Status surface

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Model label | src/lib.rs:3048 | DONE | The core status line starts with the model (`mobile/rust-core/src/view.rs` status line); rendered by `screens.rs:79`. Test: `mobile/rust-core/src/view.rs::snapshot_status_line_matches_tui_words`. |
| Cumulative tokens | src/lib.rs:3049 | DONE | Same status line (`N tok`). Same test. |
| Session cache hit percentage | src/lib.rs:3054-3056 | DONE | Same status line (`cache N%`). Same test. |
| Context percentage with the 80 percent warning | src/lib.rs:3057-3069 | PARTIAL | The core shows `ctx 41% (74k/180k)` (`view.rs`, test at line ~1994). No 80 percent warning. |
| Approval mode | src/lib.rs:3070-3073 | DONE | Last segment of the core status line; rendered at `screens.rs:79`. The "(shift+tab)" hint is absent. Test: `view.rs` status-line tests. |
| Spinner glyph while running | src/lib.rs:3041-3045 | PARTIAL | The header shows the words "running" or "idle" (`chat.rs` `header`; `view.rs` RUNNING_LABEL). No glyph. |
| Turn number while running | src/lib.rs:3074-3078 | CORE-ONLY | `turns_completed` is in the snapshot (`view.rs`) and nothing in the app shows it. |
| Current tool name while running | src/lib.rs:3079-3084 | DONE | `chat.rs` `tool_strip` shows the latest tool row. Test: `app/tests/chat.rs::live_tool_strip_swarm_rows_and_notes_render_verbatim`. |
| "esc interrupt" hint while running | src/lib.rs:3085 | MISSING | No such text in `app/src/` or `mobile/rust-core/src/view.rs`. |
| Idle hints: "? shortcuts", "ctrl+o compact/full", "esc clears" | src/lib.rs:3086-3102 | MISSING | The header shows `ctrl+o sessions   f2 settings   f1 help` (`screens.rs` `TERMINAL_NAV_HINT`). None of the old hints. |
| Session id (first 8 characters) | src/lib.rs:3126-3130 | DONE | `chat.rs` `Model.session` takes the first 8 characters of the session id; shown in the header (`chat.rs` `header`). Untested. |
| Approval prompt in the status line | src/lib.rs:3134-3144 | DONE | `chat.rs` `approval_strip`. Test: `app/tests/chat.rs::pending_approval_shows_all_three_labels`. |

## Local behaviours

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Engine runs in-process with no server | src/run.rs:304-308, 443-474 | NOT-APPLICABLE | Replaced by design: the app is always a client and starts its own `modron serve` child (`app/src/startup.rs` ~303-364; `app/README.md`). |
| First run with no key: onboarding, then mock fallback | cli/main.rs:734-771; cli/onboarding.rs:88 | DONE | `nav.rs` first-run logic opens the credentials screen once, on a blank idle session whose provider needs a sign-in. Tests: `app/src/nav.rs::first_run_asks_once_and_opens_the_screen_only_when_nothing_is_signed_in`; `nav.rs::first_run_stays_quiet_on_a_session_that_is_running`; `app/src/chat.rs` `esc_closes_the_first_run_credentials_screen`. No mock fallback concept in the app. |
| Session resume: --resume (bare = newest session; =id) | cli/main.rs:363-366, 1185-1190 | DONE | `startup.rs` `parse_args` accepts `--session` and `--resume <id>`, `--continue` (newest in any folder), `--new`. The default is the newest session in the folder (`session.rs` pick_session). A bare `--resume` with no id is an error. Tests: `app/src/startup.rs::args_parse_and_reject_conflicts`; `app/src/session.rs::latest_picks_the_newest_row_in_the_folder`. |
| Connect target: --connect | cli/main.rs:368-372 | DONE | The flag is `--url` (`startup.rs` `parse_args`). Test: `app/src/startup.rs::url_flag_connects_without_probing`. Flag name differs from the old. |
| Rewind marker honoured on resume | cli/main.rs:192, 514-515, 1249-1252 | NOT-APPLICABLE | `modron rewind` is a CLI subcommand; the TUI has no rewind key or command. |
| Goal flag --goal | cli/main.rs:358-361 | DONE | `startup.rs` `parse_args` takes `--goal`; `startup_patches` (`startup.rs` ~179-200) sends it after attach. Tests: `app/src/startup.rs::startup_patches_follow_the_flag_order`; `app/tests/d1.rs::startup_config_is_forwarded_in_order_right_after_attach`. |
| Approve-all startup flags (--yolo, --approve-all) | cli/main.rs:382-390 | DONE | `startup.rs` accepts both. Test: `startup.rs::the_config_flags_parse_into_their_fields_table_driven` (the table includes `--yolo`). |
| Other startup flags: --sandbox, --effort, --agent, --allow, --deny, --working-dir, --sessions-dir, --max-cost, --max-turns, --verbose, --attach | cli/main.rs:343-416 | PARTIAL | Forwarded: `--sandbox`, `--effort`, `--agent`, `--allow`, `--deny`, `--working-dir` and `--sessions-dir` (`startup.rs` `parse_args`, `startup_patches`). Rejected as unknown: `--max-cost`, `--max-turns`, `--verbose`, `--attach` (`startup.rs` `bad_config_values_and_unknown_flags_are_errors_with_a_reason`). No ConfigPatch field for max-cost or max-turns (grep of `crates/modron-protocol/src/lib.rs` finds none). Tests: `startup.rs::the_config_flags_parse_into_their_fields_table_driven`; `startup.rs::working_and_sessions_dirs_are_refused_when_attaching`. |
| Custom themes from the themes directory, and built-in palettes | src/lib.rs:889-897; src/theme.rs:44-124 | MISSING | `app/src/lib.rs` `install_theme` always installs the oculus theme. See `/theme`. |
| tui-dev hot reload | none | NOT-APPLICABLE | No such code in the repository. |
| TUI logs go to a file, not stderr | cli/main.rs:445 (init_tracing, called at 422) | DONE | The app writes the owned server's log to `$XDG_STATE_HOME/modron/app-serve.log` (`startup.rs` `log_path`, the server spawn). Test: `startup.rs::log_path_prefers_xdg_then_home`. |
| Session id printed on exit, with the resume command | src/run.rs:400-405 | DONE | `bin` `resume_hint` prints `session: <id> - resume with modron-app-tui --session <id>` after the terminal is restored. Test: `bin::resume_hint_names_the_session_and_the_command` (the print itself is untested). |

## Remote behaviours

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Unix socket target (default ~/.modron/serve.sock; unix:<path>) | src/remote.rs:88-91, 160-195 | DONE | `startup.rs` `probe_unix` (line 400); a running socket is preferred when no URL is set; `--socket` connects directly. Tests: `startup.rs::socket_flag_connects_to_that_unix_socket_without_probing`; `a_running_unix_socket_is_preferred_when_no_url_is_set`; `a_silent_socket_falls_back_to_the_websocket_probe`. Core: `mobile/rust-core/src/transport_native.rs` Unix variant (`#[cfg(unix)]`). |
| WebSocket to loopback (ws://) and wss:// | src/remote.rs:160-195 | DONE | `startup.rs` `probe_ws` (line 413). Test: `startup.rs::probe_accepts_a_websocket_upgrade_only`. wss validation in core (old cite client.rs:118-140, not re-verified). |
| Plain ws:// to a non-loopback host (LAN) | src/remote.rs:160-195 | DONE | `--insecure-lan` sets the core's allow_insecure. Test: `startup.rs::a_plain_ws_url_off_the_loopback_needs_insecure_lan`. |
| Device token: --token, ?token= in the URL, MODRON_DEVICE_TOKEN | src/remote.rs:36-80 | DONE | `--token` (`resolve_token`, `startup.rs` 211); `?token=` (`split_url_token`, `mobile/rust-core/src/client.rs`); `MODRON_DEVICE_TOKEN` (`device_token_from_env`, `client.rs:203`). Tests: `startup.rs::the_token_comes_from_the_flag_then_the_url_then_the_env`; `startup.rs::a_token_in_the_env_url_is_not_probed_or_kept`. |
| Protocol handshake; a mismatched server is refused | src/remote.rs:83-86 (client_hello) | DONE | `mobile/rust-core/src/client.rs:1493` compares the Welcome's protocol with `PROTOCOL_VERSION` (the previous audit cited 1216-1220; the line moved). Untested in the app. |
| Attach on connect; replay the transcript | src/run.rs:424-438, 582-601 | DONE | `session.rs` link_up (~419-436) and `list_sessions` on connect (`session.rs:422`). Test: `app/tests/terminal.rs::dropped_connection_reattaches_to_the_same_session`. |
| Live event folding into the transcript | src/run.rs:2196-2400 (ReplayFolder) | DONE | Core `fold_event` in `mobile/rust-core/src/view.rs`. Test: `app/tests/live.rs::driver_renders_the_stream_and_steers_a_typed_message`. |
| Approval decisions over the wire | src/run.rs:86-89 (LoopCmd::Approve) | DONE | `session.rs` Decide -> `client.rs:686` approve. Key route tested (`app/tests/chat.rs::a_during_pending_approval_decides_option_one`); the wire call is untested. |
| Approval resolved elsewhere clears the prompt | src/run.rs:1375-1386 | DONE | The core keeps the pending approval in the snapshot. Untested. |
| Submit and interrupt over the wire | src/run.rs:928-933, 958-968 | DONE | `session.rs` Send -> `client.rs:736` send_message; Interrupt -> `client.rs:676`. Test: `app/tests/live.rs::enter_sends_and_alt_enter_queues_the_composer_text`. |
| /fork and /tasks over the wire | src/run.rs:1058-1063 | PARTIAL (in progress) | /fork: `session.rs` Fork -> `client.rs:1113`, untested. /tasks: the wire call (`TaskStatus`, `client.rs:1136`) is being replaced by the uncommitted Tasks screen (`TaskStatusOf` per selected row). Untested. |
| /share over the wire | src/run.rs:1064-1070 | DONE | `session.rs` Share -> `client.rs:1023` publish_page. Untested. |
| /automations over the wire | src/run.rs:1071-1073 | PARTIAL (in progress) | HEAD: CORE-ONLY (core list, add and remove at `client.rs:987-1014`; no app call). Working tree: the uncommitted Automations screen sends `ListAutomations`, `RemoveAutomation` and add. Untested. |
| /new over the wire, with session adoption | src/run.rs:1074-1076 | DONE | `session.rs` NewSession -> `client.rs:1101`. Test: `app/tests/live.rs::a_share_link_adopts_the_session_its_snapshot_names`. |
| /remote-control over the wire | src/run.rs:1049-1057, 2497-2510 | DONE | `session.rs` RemoteControl -> `client.rs:1041`. Untested. |
| Mode, model and sandbox sent as config patches | src/run.rs:1077-1083, 2415-2470 | DONE | `session.rs` SetApprovalMode, SelectModel, CycleSandbox/SetSandbox. Tests: `app/tests/nav.rs::settings_shows_the_core_labels_and_enter_on_approval_mode_sends_the_mode`; `app/tests/app3.rs::enter_changes_only_the_session_model`. Sandbox over the wire untested. |
| Persona, allow and deny sent as config patches | src/run.rs:2415-2470 | DONE | `session.rs` SetPersona (`:682`), AddToolRule (`:638`) -> `client.rs:936` set_persona and `client.rs:945` set_tool_lists. Untested. Removal and pickers are missing (see the screens table). |
| Engine-local commands refused while connected, with a toast | src/run.rs:1085-1089 | NOT-APPLICABLE | No engine-local commands to refuse: each command is a core call or a screen. |
| Undecodable frame notice | src/remote.rs:109; src/run.rs:2219-2220, 2302-2305 | DONE | Core sets the notice as the last note (`mobile/rust-core/src/client.rs` decode path). Untested in the app. |
| Connection header: state and address (old: "connected to" note) | src/run.rs:587-591 | PARTIAL | `chat.rs` `header` shows the connection label, provider, running state and session id. The owned-server note is set (`bin` line 249, `OWN_SERVER_NOTE`). The old "sessions survive this TUI" note is absent. |

## Before the old TUI can be deleted

Ordered by how often a daily terminal user would hit the gap. Work types: "app" means `app/`; "core" means `mobile/rust-core` or `crates/modron-view`; "server" means `crates/modron-protocol` or `modron serve`. Items marked (in progress) are being written by another agent in the working tree.

1. **Terminal scrolling without a mouse.** The wheel, click and drag-to-copy are missing (Keys rows for mouse). Decide whether mouse capture is wanted, since it changes native selection, then add wheel scrolling. Work: app.
2. **/clear** (in progress: written in the uncommitted tree, untested). Work: app.
3. **Automations and tasks screens** (in progress): `/automations`, `/tasks`, `/worktree` bare, merge, and the wire call for each. Work: app; the core calls exist.
4. **Esc when idle** should clear the composer and resume the scroll follow. Work: app.
5. **/goal visible.** The goal is set but never shown. Work: app (status line), with core text if it should be shared.
6. **/context breakdown** and the 80 percent warning on the context percentage. Work: core (status and breakdown text), then app.
7. **/issues.** Needs a frame to the server's memory store. Work: server, core, app.
8. **Pickers for mode, sandbox, persona, and allow/deny.** Commands cycle or add, but the old pickers and removal are missing. Work: app; a persona list in the snapshot (core) for the persona picker.
9. **Auth remove.** No core call. Work: core, then app.
10. **Approval type-ahead guard.** Work: app.
11. **Status words:** the esc-to-interrupt hint, idle hints, spinner glyph and turn count. Work: core (shared words) and app.
12. **Note expiry** (in progress: uncommitted `chat.rs` code, untested). Work: app.
13. **Paste text into the input with Ctrl+V.** Currently only images attach. Work: app.
14. **macOS image clipboard** (in progress: uncommitted `clipboard.rs` JXA read, untested). Skipped at HEAD (`clipboard.rs:107`). Work: app.
15. **Startup flags** `--max-cost`, `--max-turns`, `--verbose`, `--attach`. Work: server (ConfigPatch fields for the two limits) and app.
16. **/sandbox accepts "bubblewrap"** as the old parser did. Work: app (one word).
17. **Connection header note** ("sessions survive this TUI"). Work: app, low.
18. **Welcome banner.** Work: app, low.
19. **Help tabs and scroll.** Work: app, low.
20. **Decision needed: theme switching and custom theme files** (`src/theme.rs`). Work: decision, then app. Not decided here.

## Different by decision

- **`!command` (PLAN 10.2a, rows 1 and 2; ledger SHELL-2 and PARITY-C).** Old TUI: `!cmd` runs with `sh -c` in the client's working directory (`src/run.rs:871-908`), shows the command and its output in scrollback only, and passes through no approval or deny list. New: `!cmd` output reaches the model. `!!cmd` runs privately: it is shown on every device but kept out of model context and compaction. The owner's own command is not approval-prompted, but it still honours the sandbox, the deny list, deny rules and the pre_tool veto. Guests cannot send it. The old path's behaviour is read from `src/run.rs` only (inferred for the approval step).
- **Model picker save (PLAN 10.2a, row 3).** Old TUI and new app agree: selecting a model changes the current session, and Shift+S saves the default model and provider (`src/lib.rs:1553-1573`; `app/tests/app3.rs::shift_s_saves_the_highlighted_model_and_provider_as_the_default`). Claude Code makes the choice the default; neither client does.
- **Enter while a turn runs (no PLAN entry).** Old: refused with a toast. New: steers the running turn. Listed for transparency under "Deliberately not carried over".

## Deliberately not carried over

- Ctrl+O transcript-detail toggle (src/lib.rs:925-928). It changed only the status label (src/lib.rs:3091). Ctrl+O now opens Sessions.
- Alt+Enter as newline. The app assigns Alt+Enter to queue (`app/src/help.rs` 54-62). Newline is Shift+Enter or Ctrl+J.
- Enter refused while a turn runs (src/run.rs:913-916). The app steers instead.
- `/share` publishing only the last answer (src/lib.rs:2655-2666). The shared table defines `/share` as publishing the transcript (`modron-view/src/slash.rs` lines 185-190).
- `/plan <task>` submitting only a plan-first task. The shared table defines `/plan` as a phase switch (`modron-view/src/slash.rs` 129-134). The app sets the phase, then sends the task.
- The engine running in-process with no server (src/run.rs:443-474). The app is always a client of `modron serve` (app/README.md).
- `modron rewind` and the rewind-marker resume. These are CLI features.
- tui-dev hot reload. No such code exists.
- The update notice. The old TUI never drew it.
- The context-meter bar (src/lib.rs:3863-3875). Only tests call it.
- Candidate, needs sign-off, not decided here: theme switching and custom theme files (src/theme.rs:44-124, src/lib.rs:889-897). See item 20 above.

## In the new app but not the old TUI

Each item was checked against the old TUI's parse table (`src/lib.rs` 240-269), its handler arms (2497-2716), and a search of `crates/modron-tui/src`.

- **Sessions screen.** Ctrl+O or `/sessions`; filter; `n` for a new session; `d` to delete with an inline confirm; j/k; Enter attaches (`app/src/chat.rs` `route_screen_key`; `app/src/nav.rs`; `app/src/screens.rs` `sessions_body`). The old `/sessions` arm (src/lib.rs:2553-2556) printed a pointer to the CLI, and the old TUI has no delete.
- **Settings screen** with the rows listed above, opened by F2 or `/settings`. The old `/settings` opened the auth manager (src/lib.rs:262).
- **Plan decision block.** `y` approves; `r` revises with the composer text as the note (`chat.rs` `plan_block`, `PLAN_KEYS`). The old TUI has no plan decision keys; its `/plan` submits a plan-first task (src/lib.rs:2671).
- **Phase and effort.** `/plan` and `/build` switch the phase; `/effort` sets reasoning effort (`slash.rs`; the shared table). Neither the old parse table (240-269) nor the old handler has `/build` or `/effort`.
- **Queue.** Alt+Enter queues; Backspace on an empty composer removes the last queued message; queue chips show above the composer (`chat.rs` `queue_chips`). The old TUI refuses Enter while running and has no queue.
- **Steering.** Enter while a turn runs steers it; steer echoes show above the composer (`chat.rs` `steer_rows`; `client.rs:768`).
- **Sub-agent screen.** Ctrl+A opens the first running agent; steer with Enter; kill with a y/n confirm (`nav.rs` OpenAgent; `screens.rs` `agent_panel`). The old TUI has sub-agent rows only (src/lib.rs:3329-3380); no per-agent screen or kill.
- **Confirm prompts** for session delete and agent kill (`nav.rs` AskDelete, AskKill). The old TUI has none.
- **Reconnect with backoff** (2 s doubling to 30 s) and re-attach to the same session; Esc or Ctrl+C leaves while retrying (`session.rs` `retry_delay`; `bin` Esc arm). The old TUI has no reconnect (no `reconnect` or `backoff` in `crates/modron-tui/src`).
- **Built-in demo** (`--demo`, `app/src/demo.rs`). The old TUI has none.
- **Share-link session adoption.** A `session` attach parameter adopts the session the first snapshot names (`app/tests/live.rs::a_share_link_adopts_the_session_its_snapshot_names`).
- **F1 and F2.** Help and Settings keys. The old help was `?` (the previous audit recorded this; the old key table was not re-read for F1).
- **Startup source order and fatal screen.** MODRON_URL or a unix socket, then a probe, then start a server, then a fatal screen with the two ways out (`startup.rs` `decide`; `bin` `fatal_lines`). The old TUI runs in-process.
- **Owner portal (web only).** Sign-in, session list with refresh, invites, sign-out (`app/src/portal.rs`; `nav.rs` Portal and SignIn; `screens.rs` portal_frame). Committed at HEAD. Not reachable from the terminal host.
- **Web controls.** Stop, Queue and chip Dequeue buttons; the tool strip press toggles the panel (`chat.rs` `WEB_CONTROLS`, `tool_strip`). Web only.
- **Web share settings and invites.** Create, list and revoke viewer and approver invites (`nav.rs` share section, test `the_share_section_needs_an_owner_and_a_session`; `app/src/web.rs` invites; `app/web-test.js` `testInviteFromSettings`). Web only.
- **Permission-rule display.** `/allow` or `/deny` with no argument prints the lists as a note (`session.rs:650`). Not a picker.
- **Effort setting** in the Settings rows (`nav.rs` `settings_rows`).
- **Not present (checked):** a "wake" feature (the only "wake" is the NSH frame-wake callback, `bin` run loop); a roles settings screen (the ledger's ROLE row is the server's role table, `crates/modron-server/src/dispatch.rs:388`, not a screen).
- **In progress (uncommitted):** Automations screen; Tasks screen (`/tasks`, `/worktree` bare, merge confirm); `/clear`; note expiry; macOS image paste through JXA.
- **Not started (no code in the working tree):** web attach control (the ledger's C4 and PARITY-D2 items).

## Untested DONE rows

DONE in code, with no test found at app level (the behaviour is either untested or only its mapping is tested):

1. Left / Right (caret editing is NSH's; only the key name and routing are tested).
2. Delete (key name only).
3. Multi-line Up/Down caret movement (routing only).
4. /reasoning toggle and live tail (`slash.rs` mapping only).
5. /fork (wire call).
6. /share (wire call).
7. /remote-control call (mapping only).
8. Persona, allow and deny over the wire (`session.rs` apply; no test).
9. Reasoning live tail (`chat.rs` `screen`).
10. Generative UI blocks in the app.
11. Turn-end usage line in the app.
12. Swarm summary at turn end in the app.
13. Interrupted-turn note in the app.
14. Session id in the header.
15. Print of the session id on exit (the format is tested; the print is not).
16. Protocol handshake in the app (core only).
17. Approval decisions over the wire (key route tested; the wire call is not).
18. Approval resolved elsewhere.
19. Sandbox over the wire.
20. Undecodable frame notice.

Count: 20 rows. Rows whose only test is the key or mapping, not the behaviour, are listed as such.

## Ledger claims not found in code

- **ROLE** and **C4g** were listed here by the audit and then checked by the lead: both are in the code. The "running" classification is `implies_run` in `mobile/rust-core/src/client.rs` (about line 1455, applied where frames are folded, not in `send_message`); the web tests read `location.href`, `localStorage` and `sessionStorage` for the token in `app/web-test.js` (about line 250).
- **SHELL-2:** "a saved default pins future sessions to that model" and the config-writer atomicity. These are core and config behaviours outside this audit. Not checked.
- **C7a:** "Lead smoke under a pty: ... no server left running." A manual pty run; not checkable from code.
- **Pass counts** in every ledger log line (for example "app 190", "web-test.js 27 of 27"). Not re-run; no claim here relies on them.

Ledger claims found in code, despite being listed as open or missing in the ledger: the composer height cap (PARITY-A says "no height cap"; `chat.rs` `COMPOSER_MAX_LINES = 6`); `/reasoning` in the shared table (PARITY-A says it is not; `modron-view/src/slash.rs` line 115); the `!!` path and the save control (SHELL-2 says open; `chat.rs` `submit_action` and Shift+S exist); the 18 missing commands (PARITY-D2 says missing from the shared table; the table has them now).

## Not verified

- No cargo run, no test run, no terminal or web run. Statements about key handling come from reading `app/src/bin/modron-app-tui.rs` and `chat.rs` routing.
- The uncommitted automations and tasks screens may not compile or pass; they are cited as found in the working tree at read time.
- Items marked (inferred) were not run.
- Startup time was not measured.

## Smallest remaining set for a daily terminal user

Evidence-based answer. Land these and a daily terminal user loses nothing important by switching:

1. Land the in-progress work already written in the working tree: `/clear`, the automations and tasks screens (`/automations`, `/tasks`, `/worktree`), note expiry, and the macOS image read. None is tested yet. Work: app.
2. Give the terminal a way to scroll with the wheel, or decide in writing that the terminal's own scrollback is the answer (mouse capture is off; PgUp and PgDn work). Work: app.
3. Show `/goal` in the status line, and make `/context` answer with the snapshot's numbers instead of the unknown-command notice. Work: core (status text) and app.
4. Esc on an idle composer clears the draft. Work: app.
5. Decide `/issues`: add a server frame, or drop it on purpose. The old command reads a server-side file. Work: server, core, app.

After 1 to 5 a daily user still loses: the persona, theme, and allow/deny pickers (the commands set values, but nothing removes a rule and there is no theme choice); removing a credential from `/auth`; the 80 percent context warning; the esc-to-interrupt hint, idle hints and spinner; the welcome banner; help tabs; and the startup flags `--max-cost`, `--max-turns`, `--verbose` and `--attach`.

