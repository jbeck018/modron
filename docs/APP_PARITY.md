# Modron app parity audit (old terminal UI retirement)

> **Note (2026-11-16, packet C9-TUI):** the reference crate `crates/modron-tui/` has been removed, and `modron` / `modron tui` now start the app. File:line citations to `crates/modron-tui/...` below refer to git history at commit `0ac5423` (for example `git show 0ac5423:crates/modron-tui/src/lib.rs`).

Packet PARITY-3. Read-only audit, rewritten 2026-11-16. No cargo run, no test run, no commit. Supersedes the PARITY-2 audit (rewritten 2026-11-15).

## Scope and state

- Old TUI: `crates/modron-tui/` at commit `0ac5423`, removed 2026-11-16 (all `src/...` citations below are relative to it). Frozen. Its citations were kept from PARITY-2; the ones used for changed rows were re-read and still match.
- New app: `app/` (`app/src/...`, `app/tests/...`, `app/web-test.js`, `app/README.md`, `app/NOTES.md`, `app/src/bin/modron-app-tui.rs`). Client core: `mobile/rust-core/src/{client.rs,view.rs,attach.rs}`. Shared table: `crates/modron-view/src/slash.rs`. Protocol: `crates/modron-protocol/src/lib.rs`. Server cited where a row depends on it: `crates/modron-server/src/dispatch.rs`, `crates/modron-server/src/hub.rs`. Engine: `crates/modron-core/src/engine.rs`.
- Baseline: HEAD `0567616` ("App: persona picker rows, task summaries, credential removal"). The working tree was clean when read (`git status --short` empty). Every item the PARITY-2 audit marked "(in progress)" is now committed.
- Line numbers refer to HEAD. Test names were taken from the source (`#[test]` functions in `app/src`, `app/tests`, `mobile/rust-core/src`, `crates/modron-view/src`, `crates/modron-protocol/src`). A cited test exists; it was not run.
- Not run: cargo, the Rust test suites, `app/web-test.js`, the e2e harness (`e2e/run.mjs`), and the pty driver (`e2e/tui_pty.py`). Statements about what a real terminal sends are inferred from code.
- Statuses: DONE (app file:line and a test, `file::test_name`; "untested" when no test covers the behaviour), PARTIAL (present, with the gap named), CORE-ONLY (core supports it, no app screen or key does), MISSING, NOT-APPLICABLE (reason given), DIFFERENT-BY-DECISION (PLAN 10.2a cited, both behaviours stated).
- Rows: the same 140 rows in the same order as PARITY-2, so the counts compare directly. Sections: Keys 33, Slash commands 30, Input prefixes 5, Screens and popups 28, Status surface 12, Local behaviours 12, Remote behaviours 20.

## Summary

Counts: DONE 108 (was 88, +20), PARTIAL 14 (was 27, -13), CORE-ONLY 1 (was 1), MISSING 1 (was 16, -15), NOT-APPLICABLE 7 (was 7), DIFFERENT-BY-DECISION 9 (was 1, +8). Total 140.

The big movement is the screens and commands that PARITY-2 listed as missing: pickers for mode, sandbox, effort, persona and theme; the automations and tasks screens; `/clear`; `/context`; `/goal` in the header; note expiry; the help tabs; credential removal; and the web attach control. Thirty-three rows changed status in all. Of the sixteen former MISSING rows, twelve are now DONE or DIFFERENT-BY-DECISION (six of the latter are settled by PLAN 10.2a: mouse, `/issues`, theme list), three are PARTIAL, and one is still MISSING: the approval type-ahead guard.

Remaining gaps, in short: approval type-ahead guard (MISSING); newline keys (Alt+Enter queues, Shift+Enter depends on the terminal); Enter during a run steers (no decision recorded); Esc on an idle composer does not return to the live tail; `/allow` has no menu; `/auth` arguments are dropped; `/sandbox bubblewrap` is rejected at the command; `/settings` opens Settings, not Auth (no decision recorded); the idle hint words, the turn count, the scrollback "lines above" count and four startup flags are missing.

The ledger's UPGRADE-SUM row is DONE and the REL row's FINDING line is stale (see "Ledger claims not found in code").

## Changes since PARITY-2

Rows whose status changed (old status to new status). Rows not listed kept their status.

- Enter while a turn runs (Keys): DONE -> PARTIAL
- Esc when idle: clear selection, leave scrollback, clear input (Keys): MISSING -> PARTIAL
- Mouse wheel scrolls the scrollback (Keys): MISSING -> DIFFERENT-BY-DECISION
- Click positions the input cursor (Keys): MISSING -> DIFFERENT-BY-DECISION
- Drag selects scrollback text; release copies it (Keys): PARTIAL -> DIFFERENT-BY-DECISION
- Click a tool summary or the tool strip opens the panel; click outside closes it (Keys): PARTIAL -> DIFFERENT-BY-DECISION
- /context (Slash commands): MISSING -> DONE
- /agent [name] (and /persona) (Slash commands): PARTIAL -> DONE
- /mode [ask, yolo, deny] (Slash commands): PARTIAL -> DONE
- /tasks (Slash commands): PARTIAL -> DONE
- /worktree (and /worktrees) (Slash commands): PARTIAL -> DONE
- /issues [add, done] (Slash commands): MISSING -> DIFFERENT-BY-DECISION
- /goal [condition] (Slash commands): PARTIAL -> DONE
- /theme [name] (Slash commands): MISSING -> DIFFERENT-BY-DECISION
- /automations (Slash commands): PARTIAL -> DONE
- Image attachments (Ctrl+V) and attachment chips (Input prefixes): PARTIAL -> DONE
- Theme picker (Screens and popups): MISSING -> DIFFERENT-BY-DECISION
- Sandbox picker (Screens and popups): MISSING -> DONE
- Mode picker (Screens and popups): MISSING -> DONE
- Persona picker (Screens and popups): MISSING -> DONE
- Allow and deny tool picker (Screens and popups): MISSING -> PARTIAL
- Auth manager: provider picker, method choice, masked key entry, browser or device sign-... (Screens and popups): PARTIAL -> DONE
- Help modal: three tabs (General, Commands, Shortcuts), scroll (Screens and popups): PARTIAL -> DONE
- Toast strip: transient notes that expire after 40 ticks (Screens and popups): PARTIAL -> DONE
- Input box title "working (esc to interrupt)"; rotating placeholder hints (Screens and popups): PARTIAL -> DONE
- Welcome banner (Screens and popups): MISSING -> DONE
- Context percentage with the 80 percent warning (Status surface): PARTIAL -> DONE
- Spinner glyph while running (Status surface): PARTIAL -> DONE
- "esc interrupt" hint while running (Status surface): MISSING -> DONE
- Idle hints: "? shortcuts", "ctrl+o compact/full", "esc clears" (Status surface): MISSING -> PARTIAL
- Custom themes from the themes directory, and built-in palettes (Local behaviours): MISSING -> DIFFERENT-BY-DECISION
- /fork and /tasks over the wire (Remote behaviours): PARTIAL -> DONE
- /automations over the wire (Remote behaviours): PARTIAL -> DONE

## Keys

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Enter sends the task | src/lib.rs:1116-1157 | DONE | `app/src/chat.rs` `route_key` (626) leaves Enter to the composer; `submit_action` (761) returns `UiAction::Send`; `mobile/rust-core/src/client.rs:763` `send_message`. Test: `app/tests/chat.rs::typing_and_enter_submits_and_keeps_focus`. |
| Enter while a turn runs | src/run.rs:913-916 (refused with a toast) | PARTIAL | Steers the running turn: `client.rs:763` `send_message` sends a steer when the session runs (`client.rs:795` `steer`). The old refusal and its toast are gone, and no PLAN 10.2a entry records the change (listed under "Different by decision" as an open question). Test: `app/tests/live.rs::driver_renders_the_stream_and_steers_a_typed_message`. |
| Shift+Enter or Alt+Enter inserts a newline | src/lib.rs:1103-1115 | PARTIAL | Alt+Enter queues the draft instead (`chat.rs` `queue_route` 800; `app/src/help.rs:204` lists alt+enter as queue). Shift+Enter reaches the composer as Enter with the shift modifier (`app/src/bin/modron-app-tui.rs` `key_input` 86) and the composer breaks the line. The terminal host never pushes keyboard-enhancement flags, so in most terminals a real Shift+Enter is probably indistinguishable from Enter (inferred, not run). Ctrl+J is the reliable newline (`help.rs:235`). Test: `app/tests/chat.rs::shift_enter_adds_a_line_and_enter_submits_the_whole_draft`. |
| Ctrl+J inserts a newline | src/lib.rs:942-945 | DONE | `app/src/bin/modron-app-tui.rs` `is_newline_chord` (56) maps Ctrl+J to the Shift+Enter key. Test: `app/src/bin/modron-app-tui.rs::ctrl_j_and_ctrl_enter_are_shift_enter`. |
| Backslash then Enter continues the line | src/lib.rs:1117-1125 | DONE | `app/src/chat.rs` composer `on_submit` (around 2365) turns a trailing backslash into a line break and does not submit. Test: `app/tests/chat.rs::a_trailing_backslash_continues_the_line_and_does_not_submit`. |
| Up/Down recall sent inputs (single line) | src/lib.rs:1206-1213, 1935-1959 | DONE | `chat.rs` `route_key` (664-684) recalls when the draft is empty or one line; `Host::recall` (1692). Test: `app/tests/chat.rs::up_recalls_the_last_sent_input_and_down_restores_the_draft`. |
| Up/Down move between lines of a multi-line input | src/lib.rs:1198-1205, 1963-1977 | DONE | A multi-line draft is not recalled, so Up/Down reach the NSH composer (`chat.rs` 663-664). Test: `app/src/chat.rs::up_and_down_recall_only_on_an_empty_or_recalled_single_line` covers routing only; caret movement inside NSH is untested. |
| Left / Right move the cursor | src/lib.rs:1179-1186 | DONE | `chat.rs` `route_key` sends ArrowLeft and ArrowRight to the composer (706-707); `bin` `key_name` maps them (74-75). Test: `app/src/chat.rs::the_cursor_and_delete_keys_reach_the_composer_and_never_scroll_or_recall` (routing only; caret editing is NSH's and untested). |
| Home / End | src/lib.rs:1187-1194 | DONE | `route_key` 696-697: scrolls only on an empty draft, otherwise the composer moves the caret. Test: `app/src/chat.rs::page_keys_scroll_and_home_end_scroll_only_on_an_empty_composer`. |
| Delete | src/lib.rs:1169-1178 | DONE | `bin` `key_name` maps Delete (78); `route_key` sends it to the composer (708). Test: `app/src/chat.rs::the_cursor_and_delete_keys_reach_the_composer_and_never_scroll_or_recall` (key routing only; editing is NSH's and untested). |
| Backspace edits; on an empty composer it removes the last queued message | src/lib.rs:1158-1168 | DONE | `route_key` 688-693: an empty draft with staged chips detaches the last chip first, else removes the last queued message. Tests: `app/tests/live.rs::backspace_on_an_empty_composer_dequeues_the_last_message`; `app/src/chat.rs::backspace_on_an_empty_composer_dequeues_the_last_message`. |
| Esc interrupts a running turn | src/lib.rs:1048-1050 | DONE | `chat.rs` `escape_route` (776-796) returns Interrupt when running; `client.rs:696` `interrupt`. Tests: `app/tests/chat.rs::escape_sends_interrupt`; `app/src/chat.rs::escape_interrupts_a_running_turn_and_does_nothing_idle`. No real terminal run with a turn in progress (see the riskiest-rows section). |
| Esc when idle: clear selection, leave scrollback, clear input | src/lib.rs:1240-1251 | PARTIAL | Esc on an idle composer with text clears the draft (`escape_route` 792-793; `Host::clear_draft` 1464), as PLAN 10.2a row "Esc on an idle composer" decides. Not done: the scrollback step (the old code set `history_offset = 0` before clearing the input) is absent, so Esc does not return to the live tail; the selection step has no equivalent because the terminal owns selection. Test: `app/tests/chat.rs::esc_on_an_idle_composer_clears_the_text_and_sends_nothing`. |
| Esc closes the open screen | src/lib.rs:1006-1011, 1029-1036 | DONE | `chat.rs` `route_screen_key` (841-950) and `route_list_screen_key` (958-997). Tests: `app/src/chat.rs::esc_closes_settings_and_interrupts_only_on_chat`; `app/tests/nav.rs::esc_closes_settings`; `app/tests/c3f.rs::esc_clears_the_model_filter_before_closing_the_picker`. |
| Esc on a pending approval denies it | src/lib.rs:984-986 | DONE | `chat.rs` `escape_route` 777-781 decides the last option (the third, deny). Tests: `app/src/chat.rs::escape_with_an_approval_denies_it_before_anything_else`; `app/tests/chat.rs::esc_denies_a_pending_approval_instead_of_interrupting`. The pty driver `e2e/tui_pty.py` presses Esc on a real approval prompt (scenario T1, not run here). |
| Ctrl+C aborts a running turn; quits when idle | src/lib.rs:949-956 | DONE | `bin` `ctrl_c` (113-122): the first press during a turn interrupts; a second within 2 s, or a press when idle, exits. Test: `app/src/bin/modron-app-tui.rs::ctrl_c_interrupts_once_mid_turn_and_exits_otherwise` (a pure function test). Pty scenario T1 presses Ctrl+C twice and checks exit 0; it does not assert that the first press interrupted a turn. |
| Ctrl+O toggles transcript detail (compact or full) | src/lib.rs:925-928, 3089-3098 | NOT-APPLICABLE | The old toggle changed only a status label (src/lib.rs:3091); nothing else read it. Ctrl+O opens Sessions (`chat.rs` `route_key` 650). See "Deliberately not carried over". |
| Ctrl+T expands this turn's tool activity | src/lib.rs:930-938, 3382-3409 | DONE | `route_key` 644-645 toggles; `route_tool_panel_key` (739). Tests: `app/tests/d1.rs::ctrl_t_opens_the_tool_panel_with_the_snapshot_rows_and_esc_closes_it`; web: `app/web-test.js` `testCtrlTAndShellPassthrough`. |
| Ctrl+V pastes an image as an attachment, or text into the input | src/lib.rs:946-948; src/clipboard.rs:55-64 | PARTIAL | Terminal: an image is staged (`app/src/session.rs` `paste_clipboard` 918; `app/src/clipboard.rs` `harvest` 156 runs wl-paste then xclip on Linux, and on macOS `osascript` through `harvest_with_os` 135). Clipboard text is appended to the composer (`chat.rs` `try_action` 1496, `offer_text` 1240). Web: no clipboard read; Ctrl+V only shows `CLIPBOARD_TEXT_NOTE` (`session.rs`, wasm). Tests: `app/src/clipboard.rs::an_image_on_the_clipboard_is_harvested_before_text`; `app/src/clipboard.rs::on_macos_the_image_comes_from_osascript_and_linux_tools_are_not_asked` (fake runner only); `app/tests/app3.rs::ctrl_v_asks_for_a_paste_and_a_staged_image_shows_its_chip` (stages a fake image; does not read the clipboard). |
| Pasted text enters the input verbatim, including multi-line | src/run.rs:807-815 (bracketed paste enabled at src/run.rs:173-176) | DONE | `bin` enables `EnableBracketedPaste` (`modron-app-tui.rs` 332) and forwards `Event::Paste`. Test: `app/tests/chat.rs::a_pasted_block_lands_whole_and_nothing_submits` (synthetic paste; a real terminal paste was not run). |
| Shift+Tab flips the approval mode | src/lib.rs:1229-1239 | DONE | `bin` `key_name` maps BackTab (69-71); `route_key` 698 sends `CycleMode`. Tests: `app/src/chat.rs::shift_tab_flips_the_approval_mode_and_caret_keys_stay_in_the_composer`; `app/src/bin/modron-app-tui.rs::key_map_forwards_the_keys_the_app_uses`. |
| PgUp / PgDn page the scrollback | src/lib.rs:1215-1224 | DONE | `route_key` 694-695 sends `Scroll`. Tests: `app/src/chat.rs::page_keys_scroll_and_home_end_scroll_only_on_an_empty_composer`; `app/tests/chat.rs::paging_keys_scroll_without_touching_the_composer`. |
| Mouse wheel scrolls the scrollback | src/run.rs:837-838 | DIFFERENT-BY-DECISION | PLAN 10.2a (mouse capture): the terminal host turns on no mouse capture (`modron-app-tui.rs` has no mouse enable), so the wheel does nothing there; PgUp, PgDn, Home and End scroll. Web: the wheel scrolls (`chat.rs` `wheel` 1719; `app/web-test.js` `testWheelScrollsTranscript`). Old TUI: capture on, the wheel scrolls. |
| Click positions the input cursor | src/lib.rs:1997-2043 | DIFFERENT-BY-DECISION | PLAN 10.2a (mouse capture): no click handling in the terminal. Web: the NSH composer handles its own pointer input; not checked here (untested). Old TUI: a click moved the cursor. |
| Drag selects scrollback text; release copies it | src/run.rs:816-835; src/lib.rs:2067-2149; src/clipboard.rs:27-52 | DIFFERENT-BY-DECISION | PLAN 10.2a (mouse capture): "leave selection and copy to the terminal". Terminal: native selection and copy (inferred, not run). Web: native browser selection. Old TUI: in-app selection, copied on release. |
| Click a tool summary or the tool strip opens the panel; click outside closes it | src/lib.rs:1997-2013, 2067-2072 | DIFFERENT-BY-DECISION | PLAN 10.2a (mouse capture) for the terminal, which opens the panel with Ctrl+T (row above). Web: the press toggles the panel (`chat.rs` `tool_strip` 2110-2121, under `WEB_CONTROLS` 287). No web test of the click; `app/web-test.js` `testCtrlTAndShellPassthrough` covers Ctrl+T only. |
| Tab completes the highlighted /command | src/lib.rs:1068-1072 | DONE | `route_key` 673-674 sends `CompleteSelected`. Tests: `app/src/help.rs::every_binding_routes_as_the_table_says` (the Tab binding); `app/tests/c3f.rs::typing_a_slash_shows_suggestions_and_esc_hides_them_keeping_the_text`. |
| Up/Down move the highlight in the command list | src/lib.rs:1059-1066 | DONE | `route_key` 675-676 sends `SlashMove`. Test: `app/src/help.rs::every_binding_routes_as_the_table_says` (ArrowUp and ArrowDown bindings). |
| Tab completes an @path | src/lib.rs:1094-1097 | DONE | `route_key` 673 sends `CompleteFile`; `app/src/files.rs` walks the workspace. Test: `app/tests/d1.rs::at_completion_lists_workspace_files_and_tab_splices_the_path`. |
| ? on an empty input opens help | src/lib.rs:1148-1150 | DONE | `route_key` 685-687. Test: `app/tests/c3f.rs::f1_and_a_question_mark_open_help_and_esc_closes_it`. |
| y / a / n decide the approval; a allows all for the run | src/lib.rs:974-987 | DONE | `route_key` 719-724 (`DECISION_KEYS`, `chat.rs` 280); `approval_strip` (2218). Tests: `app/src/chat.rs::approval_keys_decide_their_options_in_order`; `app/tests/chat.rs::a_during_pending_approval_decides_option_one`. |
| Approval type-ahead guard: decision keys typed as the prompt appears are ignored | src/lib.rs:962-973 | MISSING | No guard. `route_key` 719-724 decides on the first decision key while an approval is pending, with no time window. No test. |
| Shift+S on a model row saves it as the default | src/lib.rs:1553-1573; src/run.rs:1593-1601 | DONE | `route_screen_key` Models 900-903 sends `save_default`; `nav.rs` `save_default_action` (886); `mobile/rust-core/src/client.rs:1280` `persist_default_model`; the server handles `PersistDefaultModel` at `crates/modron-server/src/dispatch.rs:778` and refuses tunnels (`dispatch.rs:356`). Web control: `app/src/screens.rs` 757-763. Tests: `app/tests/app3.rs::shift_s_saves_the_highlighted_model_and_provider_as_the_default`; `app/tests/app3.rs::shift_s_on_an_unavailable_model_saves_nothing`. |

## Slash commands

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| /help and bare ? | src/lib.rs:2497-2504, 240 | DONE | `app/src/slash.rs` `run` (118) opens Help; `?` resolves through the shared alias (`crates/modron-view/src/slash.rs` 222-223). Tests: `app/tests/c3f.rs::f1_and_a_question_mark_open_help_and_esc_closes_it`; `app/src/slash.rs::every_table_command_is_mapped`. |
| /exit, /quit, /q | src/lib.rs:2496, 241 | DONE | `slash.rs` 183 maps them to `UiAction::Exit`; `chat.rs` `try_action` 1508 sets the quit flag. Test: `app/src/slash.rs::local_commands_map_to_their_actions` (the mapping; the exit path itself is untested). |
| /clear (empties the scrollback) | src/lib.rs:2505-2516, 242 | DONE | `slash.rs` 184 `"clear" => UiAction::Clear`; `chat.rs` `try_action` 1506 calls `clear_view` (1487), which sets the floor and republishes; `shown_copy` (1225) hides the rows. Test: `app/src/slash.rs::local_commands_map_to_their_actions` (mapping); the hide itself is untested (`app/src/chat.rs::clearing_past_the_end_of_the_history_hides_nothing_more` tests only the clamp). |
| /compact | src/lib.rs:2517-2524, 243 | DONE | `slash.rs` 138 -> `UiAction::Compact` -> `client.rs:908` `compact`. Tests: `app/tests/live.rs::slash_compact_sends_compact_instead_of_text`. Browser scenario W6 covers it (`e2e/run.mjs`, not run here). |
| /model [name] | src/lib.rs:2525-2545, 244 | DONE | Bare opens the picker; with a name selects (`slash.rs` 145-146). Current marker and preselect (`nav.rs` `model_rows` 1473); dimmed unavailable rows. Tests: `app/src/nav.rs::the_current_model_is_marked_and_preselected_when_the_picker_opens`; `app/tests/c3f.rs::model_picker_filters_and_enter_switches_the_model`; `app/src/slash.rs::model_bare_opens_the_picker_and_with_a_name_switches`. |
| /context | src/lib.rs:2546-2552, 245 | DONE | `slash.rs` 185 -> `UiAction::Context`; `chat.rs` `context_note` (1473) shows `context: ctx 41% (74k/180k) - N tok - cache N%` as a local note for 2 s, built by `mobile/rust-core/src/view.rs` `context_note` (204) from the snapshot. The old breakdown lines and the "compacts at 80%" sentence are not reproduced; the 80% marker is on the gauge. Test: `app/tests/chat.rs::context_command_shows_the_snapshot_numbers_as_a_note`. |
| /sessions | src/lib.rs:2553-2556, 246 | DONE | `slash.rs` 139 opens Sessions (`nav.rs` `apply` 1067, which asks for the list). Test: `app/tests/nav.rs::opening_sessions_shows_the_core_rows_and_refreshes_the_list`. |
| /sandbox [off, seatbelt, bwrap] | src/lib.rs:2557-2566, 247 | PARTIAL | Bare `/sandbox` opens the Sandbox picker (`slash.rs` 155; `nav.rs` `picker_rows` 255). `bubblewrap` is rejected at the command (`slash.rs` 158 accepts only `bwrap`, and usage is shown otherwise); the old parser accepted it. `--sandbox` accepts both (`app/src/startup.rs:96`). Test: `app/src/slash.rs::the_picker_commands_open_their_pickers_and_their_arguments_apply_directly`. |
| /reasoning (and /thinking) | src/lib.rs:2567-2575, 248 | DONE | `slash.rs` 171 maps both to `ToggleReasoning`; the live tail renders in `chat.rs` `screen` when opted in. Tests: `app/src/slash.rs::reasoning_and_thinking_both_toggle_the_reasoning_tail`; `app/src/chat.rs::the_reasoning_tail_is_the_last_two_lines_and_only_when_opted_in`. |
| /btw <question> | src/lib.rs:2576-2585, 249 | DONE | `slash.rs` 175-176: `Send(btw_task(args))`, with the shared suffix (`crates/modron-view/src/slash.rs`); bare shows the usage. Tests: `app/src/slash.rs::local_commands_map_to_their_actions` (`/btw why`); `crates/modron-view/src/slash.rs::canned_prompts_match_the_old_tui_text`. |
| /agent [name] (and /persona) | src/lib.rs:2599-2606, 250; src/lib.rs:1763-1784 (persona picker) | DONE | Bare opens the Agent picker, which lists the server's personas (`nav.rs` `picker_rows` 289-298; the personas come from `client.rs:1225` `request_personas`); with a name, `SetPersona` (`slash.rs` 182) -> `client.rs:973` `set_persona`. Tests: `app/src/nav.rs::the_agent_picker_asks_for_personas_and_lists_their_text_with_the_current_marked`; `app/tests/live.rs::agent_picker_asks_for_personas_and_enter_sends_the_chosen_one`; `app/tests/live.rs::agent_sends_the_persona_as_a_config_patch`. |
| /allow [tool], /deny [tool] | src/lib.rs:2607-2630, 251-252 | PARTIAL | With a tool: `AddToolRule` (`slash.rs` 197-198) -> `client.rs:982` `set_tool_lists`. Bare `/allow` or `/deny` prints the lists as a note (`slash.rs` 196, `session.rs` 670 `ShowToolLists`). The old bare form opened a tool menu (`open_tool_menu`, src/lib.rs:1787-1792). Removal is in the Settings rows (see the screens table). Test: `app/tests/live.rs::allow_and_deny_send_the_tool_lists_as_config_patches`. |
| /mode [ask, yolo, deny] | src/lib.rs:2623-2630, 253 | DONE | With a mode: sets it (`slash.rs` 149-151). Bare `/mode` opens the Mode picker (`slash.rs` 148; `nav.rs` `picker_rows` 243-254; the Settings approval row opens the same picker). Tests: `app/src/slash.rs::the_picker_commands_open_their_pickers_and_their_arguments_apply_directly`; `app/src/nav.rs::the_mode_picker_marks_the_current_mode_and_enter_applies_and_closes`. |
| /tasks | src/lib.rs:2631-2634, 254 | DONE | `slash.rs` 195 opens the Tasks screen (`nav.rs` 66-69 `Screen::Tasks`; `screens.rs` tasks body). Tasks come from `client.rs:1186` `task_status` and refresh every 5 s (`nav.rs` 34, `task_refresh_due` 822). Tests: `app/src/nav.rs::the_tasks_screen_lists_only_sessions_with_a_branch_and_asks_for_their_status`; `app/tests/live.rs::the_tasks_screen_renders_the_core_summary_under_the_row`. |
| /worktree (and /worktrees) | src/lib.rs:2635-2638, 255; src/run.rs:1698-1714 | DONE | Bare `/worktree` opens the Tasks screen (`slash.rs` 201); `/worktree <title>` creates a task (`slash.rs` 202 -> `client.rs:1173` `create_task`). Merge is `m` on a task row with a confirm (`nav.rs` `AskMerge`; `client.rs:1195` `merge_task`). Tests: `app/src/nav.rs::a_task_needs_a_title_and_creating_one_goes_back_to_chat`; `app/src/nav.rs::merge_asks_first_then_sends_the_merge_and_a_refresh`. |
| /issues [add, done] | src/lib.rs:2639-2650, 256; src/run.rs:1868-1936 | DIFFERENT-BY-DECISION | PLAN 10.2a ("Keep /issues?"): not carried over; it read a server-side file through the old in-process UI. The app has no arm (`slash.rs` `run` falls to the unknown notice); the shared table omits it (`crates/modron-view/src/slash.rs` test `every_tui_catalog_command_is_shared_or_tui_only` lists it as TUI-only). |
| /fork | src/lib.rs:2643-2646, 257 | DONE | `slash.rs` 194 -> `UiAction::Fork` -> `client.rs:1163` `fork_session`. Test: `app/tests/live.rs::fork_sends_a_fork_frame`. |
| /plan <task> (plan-first task) | src/lib.rs:2671-2686, 263 | DONE | `slash.rs` 172-173: bare sets the phase (`client.rs:937` `set_phase`); with a task, `UiAction::Plan(plan_task(args))`. Plan decision block: `chat.rs` `plan_block` (2184), `PLAN_KEYS` (283). Tests: `app/src/slash.rs::local_commands_map_to_their_actions` (`/plan add tests`); `app/tests/live.rs::y_approves_and_r_revises_a_pending_plan`. Browser scenario W2 covers `/plan` (not run here). |
| /review (and /code-review) | src/lib.rs:212 (catalog; handler not re-located), 2665-2670 (not re-verified) | DONE | `slash.rs` 174 -> `Send(REVIEW_TASK)`. Tests: `app/src/slash.rs::local_commands_map_to_their_actions` (`/review`); `crates/modron-view/src/slash.rs::canned_prompts_match_the_old_tui_text`. |
| /goal [condition] | src/lib.rs:2687-2701, 265 | DONE | `slash.rs` 178 -> `SetGoal` -> `client.rs:964` `set_goal`. The goal shows in the header (`chat.rs` `Model` 1106 and `header` 1998; `view.rs` `goal_label` 192). Tests: `app/tests/chat.rs::a_standing_goal_shows_in_the_header`; `mobile/rust-core/src/view.rs::goal_label_hides_an_empty_goal_and_cuts_a_long_one`. |
| /learn [title] | src/lib.rs:2702-2715, 266 | DONE | `slash.rs` 177 -> `Send(learn_task(args))`. Tests: `app/src/slash.rs::local_commands_map_to_their_actions` (`/learn Fix Parser`); `crates/modron-view/src/slash.rs::canned_prompts_match_the_old_tui_text`. |
| /theme [name] | src/lib.rs:2716-2722, 267; src/theme.rs:44-124 | DIFFERENT-BY-DECISION | PLAN 10.2a ("Theme switching"): offer what exists, Oculus dark, Oculus light and "system", with no custom theme files. Old: several built-in palettes and themes from a directory (src/theme.rs:44-124). New: `slash.rs` 186-192 opens the Theme picker or applies `dark`, `light` or `system` (`theme.rs` `Choice::parse`); the choice is saved to `app.toml` (`app/src/theme.rs` 119). Tests: `app/src/slash.rs::theme_bare_opens_the_picker_and_a_name_applies_it`; `app/src/nav.rs::the_theme_picker_applies_once_and_restore_is_not_remembered`. |
| /auth [args] | src/lib.rs:2586-2598, 268 | PARTIAL | `/auth` and `/login` open the Auth screen (`slash.rs` 141-143); the arguments are dropped (the old `add` and `manage` words, and a provider name, are not read). The screen has provider rows (`nav.rs` `auth_rows` 420), OAuth start, masked key entry, the sign-in wait with the device code (`screens.rs` 447), and removal with a confirm (`d`, `chat.rs` 918; `client.rs:1140` `auth_remove`). Tests: `app/src/nav.rs::enter_on_an_oauth_row_starts_the_sign_in_and_keeps_no_entry`; `app/src/nav.rs::d_on_a_signed_in_credential_asks_then_removes_it`; `app/tests/live.rs::d_on_a_signed_in_provider_asks_then_y_sends_auth_remove`. |
| /settings | src/lib.rs:262 (parsed as Auth in the old TUI) | PARTIAL | `slash.rs` 140 opens the Settings screen (`nav.rs` `settings_rows` 1530: model, provider, approval, phase, effort, sandbox, context, compact, Auth, theme, agent, rules, allow and deny). The old `/settings` opened the auth manager, which the app reaches with `/auth` and the Auth row. No PLAN 10.2a entry records the change (see "Different by decision"). Test: `app/tests/nav.rs::settings_shows_the_core_labels_and_enter_on_approval_mode_sends_the_mode`. |
| /new | src/lib.rs:2667-2670, 261; src/run.rs:1490-1496 | DONE | `slash.rs` 137 -> `NewSession` -> `client.rs:1151` `new_session`. Test: `app/tests/nav.rs::n_creates_a_session_and_returns_to_chat`. |
| /share | src/lib.rs:2655-2666, 260; src/run.rs:1064-1070 | DONE | `slash.rs` 203 ("share" and "publish") -> `Share` -> `client.rs:1060` `publish_page` with the whole transcript. The old command published the last answer. Test: `app/tests/live.rs::share_publishes_the_transcript_page`. |
| /remote-control [url] | src/lib.rs:2631-2634 (old citation; not re-verified), src/run.rs:1049-1057 | DONE | `slash.rs` 204-205 -> `RemoteControl` -> `client.rs:1078` `remote_control`. Tests: `app/tests/live.rs::remote_control_sends_the_bridge_url_it_was_given`; `app/src/slash.rs::local_commands_map_to_their_actions`. |
| /automations | src/lib.rs:2651-2654, 259 | DONE | `slash.rs` 199 opens the Automations screen (`nav.rs` `Screen::Automations` 66; `route_list_screen_key` `a` and `d`). List, add and remove go to `client.rs:1024`, `1033` and `1051`. Tests: `app/src/nav.rs::opening_automations_asks_for_the_list_and_shows_the_shared_rows`; `app/src/nav.rs::a_valid_automation_form_sends_the_add_then_a_refresh_and_closes`; `app/src/nav.rs::remove_asks_first_and_the_confirmation_sends_the_remove_and_a_refresh`. |
| Unknown command | src/lib.rs:2726-2729 | DONE | `slash.rs` `run` fallback returns `SLASH_UNKNOWN_LABEL`. Test: `app/src/slash.rs::unknown_words_get_the_shared_notice`. |
| Aliases: /? /compress /login /publish /rc /thinking /persona /worktrees /code-review | src/lib.rs:185-227, 240-269 | DONE | Shared aliases in `crates/modron-view/src/slash.rs` (`SlashSpec::aliases`, `slash_lookup` 250); `slash.rs` `run` resolves them (`compress` 142, `login` 141, `publish` 203, `rc` 204, `thinking` 171, `persona` 179, `worktrees` 201, `code-review` 174). Tests: `crates/modron-view/src/slash.rs::every_alias_resolves_to_its_command`; `aliases_match_the_old_tui_alias_rows`; `app/src/slash.rs::local_commands_map_to_their_actions` (`/login`, `/rc`, `/q`). |

## Input prefixes

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| !command shell passthrough (runs in the workspace, or on the client when remote) | src/run.rs:871-908, 1839-1841 | DIFFERENT-BY-DECISION | See "Different by decision". `!cmd` goes to `UiAction::Shell` (`chat.rs` `submit_action` 765-766) -> `client.rs:842` `shell`; the server runs it through `engine.rs` `run_user_shell`, with the deny list, rules, sandbox and hooks. Tests: `app/tests/d1.rs::a_bang_line_is_a_shell_action_and_never_a_submit`; `app/src/chat.rs::bang_is_shared_and_double_bang_is_private`. Browser scenario W4 covers it (not run here). |
| !command output: stdout, stderr, exit code; capped at 10 KiB | src/run.rs:1307-1327, 1839-1857 (cap_output at 1847-1857) | DONE | Cap in the engine: `crates/modron-core/src/engine.rs:124` `USER_SHELL_MAX_BYTES` = 10 KiB. The app renders the command header and dimmed output rows (`chat.rs` `shell_rows`). Tests: `app/src/chat.rs::shell_rows_put_the_command_first_and_dim_the_output`; `app/tests/shell.rs::shell_rows_render_the_command_header_and_their_output`. The cap has no app-level test. |
| @path mention: popup, Up/Down, Tab completes | src/lib.rs:1077-1100, 2170-2248, 3706-3730 | DONE | `app/src/suggest.rs` (`for_draft`, `splice_file`), `app/src/files.rs` (gitignore-aware walk with a cap), `chat.rs` `file_mode`. Tests: `app/tests/d1.rs::at_completion_lists_workspace_files_and_tab_splices_the_path`; `app/src/suggest.rs::at_must_start_a_token`; `app/src/files.rs::walk_respects_gitignore_and_skips_the_heavy_dirs_and_hidden_entries`. The web keeps `@` as text. |
| Image attachments (Ctrl+V) and attachment chips | src/lib.rs:2984-3018, 3778-3787 | DONE | Terminal: Ctrl+V stages a chip (see Ctrl+V above). Web: an attach control opens the browser file chooser (`chat.rs` `composer_controls` 2270-2291, `ATTACH_LABEL` 2258, `open_file_picker` 2263; `app/src/web.rs` 479). Chips render above the composer (`attachment_rows` 2048; `attachment_rows`); Backspace drops the last chip first. Submit sends the staged images (`client.rs:679` `submit_with`); a message with attachments is refused while the session runs (`client.rs` 767-769). Tests: `app/tests/app3.rs::ctrl_v_asks_for_a_paste_and_a_staged_image_shows_its_chip`; `app/src/chat.rs::chips_show_name_and_the_shared_size`; web: `app/web-test.js` `testAttachSendsOneImage`. |
| Dropped file paths arrive as text | src/run.rs:807-815 | DONE | A dropped path arrives as bracketed paste text (`bin` `Event::Paste`) and attaches nothing. Test: `app/tests/chat.rs::a_pasted_block_lands_whole_and_nothing_submits`. |

## Screens and popups

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Scrollback of committed transcript, wrapped | src/lib.rs:2265-2296, 3308-3325 | DONE | `chat.rs` `transcript_lines` (1192) and `style_of` (1173) over core history rows (`mobile/rust-core/src/view.rs`). Test: `app/tests/chat.rs::transcript_rows_render_in_order`. |
| Scrollback paging and the "lines above" indicator | src/lib.rs:1215-1224, 3309-3316 | PARTIAL | Paging works (Keys). The "new output below" indicator exists (`chat.rs` `NEW_OUTPUT_BELOW_LABEL`, 2424, shown while scrolled up). No "lines above" count. Test: `app/tests/chat.rs::paging_keys_scroll_without_touching_the_composer`. |
| Live answer tail while streaming | src/lib.rs:3288-3295, 2734-2745 | DONE | `chat.rs` `screen` (2323) draws the core streaming text. Test: `app/tests/chat.rs::streaming_text_appears_and_clears`. |
| Live reasoning tail (opt-in) | src/lib.rs:3279-3287, 2737-2741 | DONE | `Model.reasoning` (`chat.rs` `from_snapshot` 1103) fills only when `show_reasoning`; drawn in `screen`. Tests: `app/src/chat.rs::the_reasoning_tail_is_the_last_two_lines_and_only_when_opted_in`; `mobile/rust-core/src/view.rs::snapshot_seeds_reasoning_only_when_opted_in`. |
| Live tool strip: turn summary and latest rows | src/lib.rs:3411-3429 | DONE | `chat.rs` `tool_strip` (2110) shows the core turn summary and the last rows. Test: `app/tests/chat.rs::live_tool_strip_swarm_rows_and_notes_render_verbatim`. |
| Expanded tool panel: all events, scroll, Esc closes | src/lib.rs:3382-3409, 2825-2830 | DONE | `chat.rs` `tool_panel` (2134) and `route_tool_panel_key` (739). Tests: `app/tests/d1.rs::ctrl_t_opens_the_tool_panel_with_the_snapshot_rows_and_esc_closes_it`; `app/tests/d1.rs::the_panel_pages_and_jumps_with_the_transcript_keys`. |
| Sub-agent rows above the input | src/lib.rs:3329-3380, 2762-2793 | DONE | `chat.rs` `swarm_slots` (2465) over core `subagent_rows`. Tests: `app/tests/chat.rs::live_tool_strip_swarm_rows_and_notes_render_verbatim`; `app/tests/c3f.rs::the_agent_screen_steers_and_kills_with_confirmation`. |
| Approval strip: tool, args summary, three options | src/lib.rs:3431-3440 | DONE | `chat.rs` `approval_strip` (2218). Test: `app/tests/chat.rs::pending_approval_shows_all_three_labels`. |
| Command list above the input: filtered, windowed | src/lib.rs:3669-3705 | DONE | `chat.rs` `slash_list` (2500) over `slash.rs` `suggestions_for`. Test: `app/tests/c3f.rs::typing_a_slash_shows_suggestions_and_esc_hides_them_keeping_the_text`. |
| Model picker: filter, current marker, provider and window detail, no-key marking | src/lib.rs:1643-1676, 3584-3630 | DONE | `nav.rs` `model_rows` (1473) marks the current model and preselects it; unavailable rows are dim (`screens.rs` `model_row`) and Enter ignores them. Tests: `app/src/nav.rs::the_current_model_is_marked_and_preselected_when_the_picker_opens`; `app/src/nav.rs::the_model_picker_lists_the_catalog_and_skips_unavailable_rows`; `app/tests/c3f.rs::model_picker_filters_and_enter_switches_the_model`. |
| Theme picker | src/lib.rs:1679-1707 | DIFFERENT-BY-DECISION | PLAN 10.2a ("Theme switching"): the picker offers Oculus dark, Oculus light and system (`nav.rs` `picker_rows` 299-308 over `CHOICES`). The old picker listed the built-in palettes (src/theme.rs:44-124). Test: `app/src/nav.rs::the_theme_picker_applies_once_and_restore_is_not_remembered`. |
| Sandbox picker | src/lib.rs:1711-1737 | DONE | `nav.rs` `picker_rows` 255-263 (Off, Seatbelt, Bubblewrap; the current one marked). Opened by bare `/sandbox` (`slash.rs` 155) and the Settings sandbox row (`nav.rs` 1559-1562). Tests: `app/src/nav.rs::a_picker_opened_from_settings_closes_back_to_settings`; `app/src/slash.rs::the_picker_commands_open_their_pickers_and_their_arguments_apply_directly`. |
| Mode picker | src/lib.rs:1740-1760 | DONE | `nav.rs` `picker_rows` 243-254 (ask, yolo, deny; current marked). Opened by bare `/mode` and the Settings approval row. Tests: `app/src/nav.rs::the_mode_picker_marks_the_current_mode_and_enter_applies_and_closes`; `app/tests/nav.rs::settings_shows_the_core_labels_and_enter_on_approval_mode_sends_the_mode`. Browser scenario W3 covers the Settings picker (not run here). |
| Persona picker | src/lib.rs:1763-1784 | DONE | `nav.rs` `picker_rows` 283-298 lists the personas the core fetched (`client.rs:1225`; `PersonaRowView` in `view.rs` (551)); with none, the row is the shared "none" and is not selectable. Tests: `app/src/nav.rs::the_agent_picker_asks_for_personas_and_lists_their_text_with_the_current_marked`; `app/tests/live.rs::the_agent_picker_renders_the_personas_the_snapshot_names`. |
| Allow and deny tool picker | src/lib.rs:1787-1820 | PARTIAL | No picker. Rules and the allow and deny lists appear as Settings rows; `d` on one asks y or n, then removes it (`nav.rs` `settings_rows` 1464-1475 `removable`; `chat.rs` 874-876). Adding is `/allow <tool>` or `/deny <tool>`. Tests: `app/src/nav.rs::d_asks_before_removing_a_rule_and_the_confirmation_sends_the_remove`; `app/src/nav.rs::allow_and_deny_entries_are_removable_through_the_tool_lists`. |
| Auth manager: provider picker, method choice, masked key entry, browser or device sign-in wait, remove | src/lib.rs:416-496, 1264-1485, 3448-3581 | DONE | Provider rows (`nav.rs` `auth_rows` 420, with status from the core); an OAuth row starts the browser or device sign-in (`nav.rs` 1923 test); a key row opens the masked entry (`nav.rs` 1931 test); the sign-in wait and device code are drawn in `screens.rs` 447; removal of a signed-in row asks first (`nav.rs` `AskAuthRemove`). Tests: `app/src/nav.rs::enter_on_an_oauth_row_starts_the_sign_in_and_keeps_no_entry`; `app/src/chat.rs::the_secret_key_never_prints`; `app/src/nav.rs::d_on_a_signed_in_credential_asks_then_removes_it`. The method choice is the row choice, not a separate prompt. |
| Help modal: three tabs (General, Commands, Shortcuts), scroll | src/lib.rs:275-412, 3635-3667 | DONE | `app/src/help.rs` `HelpTab` (529: General, Commands, Shortcuts); Tab, Left and Right step the tabs and Up, Down, PgUp, PgDn, Home and End scroll (`chat.rs` `route_screen_key` 847-859). Tests: `app/src/help.rs::every_tab_has_rows_and_the_labels_are_the_old_three`; `app/src/help.rs::the_tabs_step_in_order_and_wrap_at_both_ends`; `app/src/nav.rs::the_help_tabs_switch_and_the_rows_scroll_within_the_tab`. |
| Toast strip: transient notes that expire after 40 ticks | src/lib.rs:905-908, 2410-2439, 3732-3745 | DONE | `chat.rs` `NOTE_TTL` (1215, 2 s, the old 40 ticks at 50 ms), `note_expired` (1218), `expire_note` (1447) drops the last note after the TTL; an error never expires. `note_rows` (2102) shows the last note and last error (`NOTE_SLOTS` 361). Test: `app/src/chat.rs::a_note_lasts_two_seconds_and_an_error_never_expires`. |
| Input box title "working (esc to interrupt)"; rotating placeholder hints | src/lib.rs:3747-3823 | DONE | `chat.rs` `hint_text` (1866) returns "esc to interrupt" while running (`mobile/rust-core/src/view.rs` `RUNNING_HINT` 89); `IDLE_HINTS` (view.rs) rotate in the empty composer (`idle_hint`). Header shows them (`header` 1970). Tests: `app/src/chat.rs::the_header_hint_and_spinner_follow_the_running_state_only`; `mobile/rust-core/src/view.rs::running_spinner_and_rotating_idle_hints_follow_the_clock_tick`. |
| Input box caret and scrolling input | src/lib.rs:1878-1930, 3844-3857 | PARTIAL | The composer is capped at 6 lines (`chat.rs` `COMPOSER_MAX_LINES` 368). Caret and scrolling are NSH's. Multi-line and arrow keys reach the composer. No visual test. |
| Generative UI blocks rendered as aligned ASCII | src/lib.rs:2881-2898 | DONE | The core renders GenerativeUi with `modron_view::render_ascii` (`mobile/rust-core/src/view.rs` `entry_lines` (744)); the app shows the core lines. Untested in the app. |
| Turn-end usage line (tokens, turns, cache) | src/lib.rs:2907-2918 | DONE | Core Usage entry (`view.rs` `entry_lines`: "N tok - cache N%"); `chat.rs` `style_of` (1173) gives it the meta style. Untested in the app. |
| Swarm summary at turn end | src/lib.rs:2762-2793 | DONE | Core Swarm entry through `transcript_lines`. Untested in the app. |
| Tool-activity summary line committed per turn | src/lib.rs:2799-2821 | DONE | Core ToolSummary entry. Test: `app/tests/chat.rs::turn_summary_and_tool_rows_leave_with_the_turn`. Click-to-expand is in the tool panel row. |
| Interrupted-turn note | src/lib.rs:2925-2940 | DONE | The core keeps the abort summary as the last note; the app shows it. Test: `app/tests/live.rs::an_interrupted_turn_note_stays_in_the_notes_row`. |
| Welcome banner | src/run.rs:541-575 | DONE | `view.rs` `welcome_lines` (134) for a blank idle session; `chat.rs` `from_snapshot` (1119-1131) shows it. Tests: `app/src/chat.rs::a_blank_idle_session_shows_the_welcome_banner`; `mobile/rust-core/src/view.rs::the_welcome_banner_names_the_app_the_model_and_the_keys`. |
| Update notice | src/lib.rs:588 (field only; view() never reads it) | NOT-APPLICABLE | The old TUI never draws the notice, so there is nothing to match. The CLI check remains (cli/main.rs:2915). |
| Context-meter bar | src/lib.rs:3863-3875 | NOT-APPLICABLE | Only tests call `context_meter` (crates/modron-tui/tests/state.rs:399-404). |

## Status surface

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Model label | src/lib.rs:3048 | DONE | The core status line starts with the model (`mobile/rust-core/src/view.rs` status line, 1196-1215); drawn by `screens.rs` 102. Test: `mobile/rust-core/src/view.rs::snapshot_status_line_matches_tui_words`. |
| Cumulative tokens | src/lib.rs:3049 | DONE | Same status line (`N tok`). Same test. |
| Session cache hit percentage | src/lib.rs:3054-3056 | DONE | Same status line (`cache N%`). Same test. |
| Context percentage with the 80 percent warning | src/lib.rs:3057-3069 | DONE | Core: `view.rs` `CONTEXT_WARNING_PCT` (155) and `context_label` (167) append `!` at 80 percent or more; the status line carries it (`view.rs` 1215). App: `screens.rs` 102 draws the status line. Core test: `mobile/rust-core/src/view.rs::context_label_warns_from_eighty_percent_with_the_old_marker`. Untested in the app. |
| Approval mode | src/lib.rs:3070-3073 | DONE | Last segment of the core status line; drawn at `screens.rs` 102. The "(shift+tab)" hint is not shown. Test: `mobile/rust-core/src/view.rs::status_line_shows_plan_phase_and_context_only_when_relevant`. |
| Spinner glyph while running | src/lib.rs:3041-3045 | DONE | `chat.rs` `spinner_text` (1877) draws the frame while running and nothing when idle (`view.rs` `SPINNER_FRAMES` 94); the terminal loop wakes every 120 ms only while a turn runs or tasks are open (`modron-app-tui.rs` 346-356). Tests: `app/src/chat.rs::the_header_hint_and_spinner_follow_the_running_state_only`; `mobile/rust-core/src/view.rs::running_spinner_and_rotating_idle_hints_follow_the_clock_tick`. |
| Turn number while running | src/lib.rs:3074-3078 | CORE-ONLY | `turns_completed` is in the snapshot (`view.rs` 998, 1162) and nothing in the app shows it. |
| Current tool name while running | src/lib.rs:3079-3084 | DONE | `chat.rs` `tool_strip` (2110) shows the latest tool row. Test: `app/tests/chat.rs::live_tool_strip_swarm_rows_and_notes_render_verbatim`. |
| "esc interrupt" hint while running | src/lib.rs:3085 | DONE | `chat.rs` `hint_text` (1866) returns `RUNNING_HINT` ("esc to interrupt") while running; drawn in `header` (1970). Test: `app/src/chat.rs::the_header_hint_and_spinner_follow_the_running_state_only`. |
| Idle hints: "? shortcuts", "ctrl+o compact/full", "esc clears" | src/lib.rs:3086-3102 | PARTIAL | Idle shows the shared rotating hints (`view.rs` `IDLE_HINTS`, "try /model ...", "try !command ..."), not the three old hints. The header's navigation hint is `ctrl+o sessions   f2 settings   f1 help` (`screens.rs` `TERMINAL_NAV_HINT` 58). Test: `app/src/screens.rs::the_terminal_hint_names_every_opener`. |
| Session id (first 8 characters) | src/lib.rs:3126-3130 | DONE | `chat.rs` `Model.session` takes the first 8 characters; shown in `header` (1970). Test: `app/src/chat.rs::the_header_shows_the_first_eight_characters_of_the_session`. |
| Approval prompt in the status line | src/lib.rs:3134-3144 | DONE | `chat.rs` `approval_strip` (2218). Test: `app/tests/chat.rs::pending_approval_shows_all_three_labels`. |

## Local behaviours

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Engine runs in-process with no server | src/run.rs:304-308, 443-474 | NOT-APPLICABLE | Replaced by design: the app is always a client and starts its own `modron serve` child (`app/src/startup.rs` `decide` 266; `app/README.md`). |
| First run with no key: onboarding, then mock fallback | cli/main.rs:734-771; cli/onboarding.rs:88 | DONE | `nav.rs` `first_run` (948) opens the credentials screen once, on a blank idle session whose provider needs a sign-in. Tests: `app/src/nav.rs::first_run_asks_once_and_opens_the_screen_only_when_nothing_is_signed_in`; `app/src/nav.rs::first_run_stays_quiet_on_a_session_that_is_running`; `app/src/chat.rs::esc_closes_the_first_run_credentials_screen`. No mock fallback concept in the app. |
| Session resume: --resume (bare = newest session; =id) | cli/main.rs:363-366, 1185-1190 | DONE | `startup.rs` `parse_args` (103) accepts `--session`, `--resume <id>`, `--continue` and `--new`; the default is the newest session in the folder (`session.rs` `pick_session` 289). Tests: `app/src/startup.rs::args_parse_and_reject_conflicts`; `app/src/session.rs::latest_picks_the_newest_row_in_the_folder`. |
| Connect target: --connect | cli/main.rs:368-372 | DONE | The flag is `--url` (`startup.rs` `parse_args`). Test: `app/src/startup.rs::url_flag_connects_without_probing`. Flag name differs from the old. |
| Rewind marker honoured on resume | cli/main.rs:192, 514-515, 1249-1252 | NOT-APPLICABLE | `modron rewind` is a CLI subcommand; the TUI has no rewind key or command. |
| Goal flag --goal | cli/main.rs:358-361 | DONE | `startup.rs` `parse_args` takes `--goal`; `startup_patches` (179) sends it after attach. Tests: `app/src/startup.rs::startup_patches_follow_the_flag_order`; `app/tests/d1.rs::startup_config_is_forwarded_in_order_right_after_attach`. |
| Approve-all startup flags (--yolo, --approve-all) | cli/main.rs:382-390 | DONE | `startup.rs` accepts both. Test: `app/src/startup.rs::the_config_flags_parse_into_their_fields_table_driven` (the table includes `--yolo`). |
| Other startup flags: --sandbox, --effort, --agent, --allow, --deny, --working-dir, --sessions-dir, --max-cost, --max-turns, --verbose, --attach | cli/main.rs:343-416 | PARTIAL | Forwarded: `--sandbox`, `--effort`, `--agent`, `--allow`, `--deny`, `--working-dir`, `--sessions-dir` (`startup.rs` `parse_args`, `startup_patches`). Rejected as unknown: `--max-cost`, `--max-turns`, `--verbose`, `--attach` (`startup.rs` `bad_config_values_and_unknown_flags_are_errors_with_a_reason`). No ConfigPatch field for max-cost or max-turns (grep of `crates/modron-protocol/src/lib.rs` finds none). Tests: `app/src/startup.rs::the_config_flags_parse_into_their_fields_table_driven`; `app/src/startup.rs::working_and_sessions_dirs_are_refused_when_attaching`. |
| Custom themes from the themes directory, and built-in palettes | src/lib.rs:889-897; src/theme.rs:44-124 | DIFFERENT-BY-DECISION | PLAN 10.2a ("Theme switching"): no custom theme files for now; NSH can swap tokens at runtime, so another palette is a data change. The app has dark, light and system (`app/src/theme.rs`); the choice is remembered in `app.toml` (119) on the terminal and in localStorage on the web. Test: `app/src/theme.rs::names_round_trip_and_the_short_forms_parse`. |
| tui-dev hot reload | none | NOT-APPLICABLE | No such code in the repository. |
| TUI logs go to a file, not stderr | cli/main.rs:445 (init_tracing, called at 422) | DONE | The app writes the owned server's log to `$XDG_STATE_HOME/modron/app-serve.log` (`startup.rs` `log_path` 450). Test: `app/src/startup.rs::log_path_prefers_xdg_then_home`. |
| Session id printed on exit, with the resume command | src/run.rs:400-405 | DONE | `modron-app-tui.rs` `exit_lines` (126) and `resume_hint` (130) print `session: <id> - resume with modron-app-tui --session <id>` after the terminal is restored. Tests: `app/src/bin/modron-app-tui.rs::the_resume_line_prints_on_exit_only_for_an_attached_session`; `app/src/bin/modron-app-tui.rs::resume_hint_names_the_session_and_the_command`. The pty scenario T1 checks the printed line (not run here). |

## Remote behaviours

| feature | old TUI (file:line) | new app status | evidence or gap |
|---|---|---|---|
| Unix socket target (default ~/.modron/serve.sock; unix:<path>) | src/remote.rs:88-91, 160-195 | DONE | `startup.rs` `probe_unix` (400); a running socket is preferred when no URL is set; `--socket` connects directly. Tests: `app/src/startup.rs::socket_flag_connects_to_that_unix_socket_without_probing`; `app/src/startup.rs::a_running_unix_socket_is_preferred_when_no_url_is_set`; `app/src/startup.rs::a_silent_socket_falls_back_to_the_websocket_probe`. Core: `mobile/rust-core/src/transport_native.rs` Unix variant. |
| WebSocket to loopback (ws://) and wss:// | src/remote.rs:160-195 | DONE | `startup.rs` `probe_ws` (413). Test: `app/src/startup.rs::probe_accepts_a_websocket_upgrade_only`. wss validation in core (old cite client.rs:118-140, not re-verified). |
| Plain ws:// to a non-loopback host (LAN) | src/remote.rs:160-195 | DONE | `--insecure-lan` sets the core's allow_insecure. Test: `app/src/startup.rs::a_plain_ws_url_off_the_loopback_needs_insecure_lan`. |
| Device token: --token, ?token= in the URL, MODRON_DEVICE_TOKEN | src/remote.rs:36-80 | DONE | `--token` (`resolve_token` 211); `?token=` (`split_url_token`, `mobile/rust-core/src/client.rs`); `MODRON_DEVICE_TOKEN` (`device_token_from_env`, `client.rs:203`). Tests: `app/src/startup.rs::the_token_comes_from_the_flag_then_the_url_then_the_env`; `app/src/startup.rs::a_token_in_the_env_url_is_not_probed_or_kept`. |
| Protocol handshake; a mismatched server is refused | src/remote.rs:83-86 (client_hello) | DONE | `mobile/rust-core/src/client.rs:1598` compares the Welcome's protocol with `PROTOCOL_VERSION`. Untested: no test covers the refusal path. |
| Attach on connect; replay the transcript | src/run.rs:424-438, 582-601 | DONE | `session.rs` `link_up` (508) and `list_sessions` on connect. Tests: `app/tests/terminal.rs::dropped_connection_reattaches_to_the_same_session`; browser scenarios S1 and S6 cover replay (not run here). |
| Live event folding into the transcript | src/run.rs:2196-2400 (ReplayFolder) | DONE | Core `fold_event` in `mobile/rust-core/src/view.rs`. Test: `app/tests/live.rs::driver_renders_the_stream_and_steers_a_typed_message`. |
| Approval decisions over the wire | src/run.rs:86-89 (LoopCmd::Approve) | DONE | `session.rs` `apply` (571) sends Decide -> `client.rs:706` `approve`. Tests: `app/tests/live.rs::y_sends_the_approval_decision_over_the_wire`; `app/tests/chat.rs::a_during_pending_approval_decides_option_one`. |
| Approval resolved elsewhere clears the prompt | src/run.rs:1375-1386 | DONE | The core keeps the pending approval in the snapshot; the app shows what it holds. Test: `app/tests/live.rs::an_approval_that_leaves_the_snapshot_clears_the_prompt`. |
| Submit and interrupt over the wire | src/run.rs:928-933, 958-968 | DONE | `session.rs` Send -> `client.rs:763` `send_message`; Interrupt -> `client.rs:696` `interrupt`. Tests: `app/tests/live.rs::enter_sends_and_alt_enter_queues_the_composer_text`; `app/tests/chat.rs::escape_sends_interrupt`. |
| /fork and /tasks over the wire | src/run.rs:1058-1063 | DONE | /fork: `session.rs` Fork -> `client.rs:1163`. Test: `app/tests/live.rs::fork_sends_a_fork_frame`. /tasks: `client.rs:1186` `task_status` and `refresh_task_statuses` (1237, batches of 10). Test: `app/tests/live.rs::the_tasks_screen_renders_the_core_summary_under_the_row`. |
| /share over the wire | src/run.rs:1064-1070 | DONE | `session.rs` Share -> `client.rs:1060` `publish_page`. Test: `app/tests/live.rs::share_publishes_the_transcript_page`. |
| /automations over the wire | src/run.rs:1071-1073 | DONE | The Automations screen sends `list_automations`, `add_automation` and `remove_automation` (`client.rs:1024`, `1033`, `1051`). Tests: `app/src/nav.rs::opening_automations_asks_for_the_list_and_shows_the_shared_rows`; `mobile/rust-core/src/view.rs::automation_row_shows_name_interval_and_last_run`. |
| /new over the wire, with session adoption | src/run.rs:1074-1076 | DONE | `session.rs` NewSession -> `client.rs:1151` `new_session`. Tests: `app/tests/nav.rs::n_creates_a_session_and_returns_to_chat`; `app/tests/live.rs::a_share_link_adopts_the_session_its_snapshot_names`. |
| /remote-control over the wire | src/run.rs:1049-1057, 2497-2510 | DONE | `session.rs` RemoteControl -> `client.rs:1078` `remote_control`. Test: `app/tests/live.rs::remote_control_sends_the_bridge_url_it_was_given`. |
| Mode, model and sandbox sent as config patches | src/run.rs:1077-1083, 2415-2470 | DONE | `session.rs` SetApprovalMode, SelectModel, SetSandbox -> `client.rs:995` `set_config`. Tests: `app/tests/nav.rs::settings_shows_the_core_labels_and_enter_on_approval_mode_sends_the_mode`; `app/tests/app3.rs::enter_changes_only_the_session_model`; `app/tests/live.rs::sandbox_sends_the_policy_as_a_config_patch`. |
| Persona, allow and deny sent as config patches | src/run.rs:2415-2470 | DONE | `session.rs` SetPersona and AddToolRule -> `client.rs:973` `set_persona` and `client.rs:982` `set_tool_lists`. Tests: `app/tests/live.rs::agent_sends_the_persona_as_a_config_patch`; `app/tests/live.rs::allow_and_deny_send_the_tool_lists_as_config_patches`. Removal: `mobile/rust-core/src/view.rs::removing_a_rule_sends_the_rest_as_wire_rules_in_order`. |
| Engine-local commands refused while connected, with a toast | src/run.rs:1085-1089 | NOT-APPLICABLE | No engine-local commands to refuse: each command is a core call or a screen. |
| Undecodable frame notice | src/remote.rs:109; src/run.rs:2219-2220, 2302-2305 | DONE | Core sets the notice as the last note (`mobile/rust-core/src/client.rs` decode path). Test: `app/tests/live.rs::an_undecodable_frame_shows_the_shared_notice`. |
| Connection header: state and address (old: "connected to" note) | src/run.rs:587-591 | PARTIAL | `chat.rs` `header` (1970) shows the connection label, provider, running state and session id. The owned-server note is set (`bin` `OWN_SERVER_NOTE`). The old "sessions survive this TUI" note is absent. |

## Before the old TUI can be deleted

Ordered by how often a daily terminal user would hit the gap. Work types: "app" means `app/`; "core" means `mobile/rust-core` or `crates/modron-view`; "server" means `crates/modron-protocol` or `crates/modron-server`; "decision" means a LEDGER or PLAN entry is needed first. Only rows that are PARTIAL, CORE-ONLY or MISSING are listed.

1. **Approval type-ahead guard** (MISSING; app). A y, a or n typed as the approval prompt appears decides it. Hit on every approval. Safety-relevant; the old guard is src/lib.rs:962-973. Work: app.
2. **Newline keys** (PARTIAL; app and decision). Alt+Enter queues rather than inserting a newline; a real Shift+Enter is probably not distinguishable in most terminals (no keyboard-enhancement flags are pushed). Work: app (push the enhancement flags, or document Ctrl+J and decide Alt+Enter with a PLAN 10.2a entry).
3. **Enter while a turn runs** (PARTIAL; decision). The app steers where the old TUI refused with a toast. Work: decision (LEDGER entry), then nothing or app.
4. **Esc on an idle composer** (PARTIAL; app). It clears the draft (decided) but does not return the scrollback to the live tail first. Work: app.
5. **"Lines above" count** in the scrollback (PARTIAL; app). Work: app.
6. **/allow and /deny with no argument** (PARTIAL; app). They print the lists; the old form opened a tool menu. Removal exists in Settings. Work: app.
7. **/auth arguments** (PARTIAL; app). `add`, `manage` and provider names are dropped. Work: app.
8. **/settings target** (PARTIAL; decision, then app). The old command opened Auth; the app opens Settings. Work: decision; then app if Auth is wanted.
9. **/sandbox bubblewrap** (PARTIAL; app). Rejected at the command, accepted at the flag. Work: app (one word).
10. **Turn number while running** (CORE-ONLY; app). `turns_completed` is in the snapshot. Work: app.
11. **Idle hint words** (PARTIAL; app). "? shortcuts", "ctrl+o compact/full" and "esc clears" are not shown; the shared rotating hints are. Work: core (shared words) and app.
12. **Startup flags** `--max-cost`, `--max-turns`, `--verbose`, `--attach` (PARTIAL; server and app). No ConfigPatch field for the two limits. Work: server (ConfigPatch fields), then app.
13. **Connection header note** ("sessions survive this TUI") (PARTIAL; app, low). Work: app.
14. **Ctrl+V on the web** (PARTIAL; app, web only). Shows a note; the browser has no clipboard read here. Work: app.
15. **Caret and input scroll** (PARTIAL; app). NSH draws the caret; no visual test. Work: app (visual test).

Crate dependency: `crates/modron-cli` depends on `modron-tui` (`crates/modron-cli/Cargo.toml:18`) and calls `modron_tui::run_with` at `crates/modron-cli/src/main.rs:705` (`modron tui`) and at 769 and 816 (read, not run). Deleting the crate is therefore a change to the `modron` command, not only a removal; REL says bare `modron` is unchanged. Resolved 2026-11-16 (C9-TUI): the dependency is gone, and bare `modron` and `modron tui` start the app.

## Different by decision

Each entry cites PLAN 10.2a (the table under "Decisions made under this rule", `PLAN.md` 313-331).

- **`!command` (PLAN 10.2a, rows 1 and 2; ledger SHELL-2 and PARITY-C).** Old TUI: `!cmd` runs with `sh -c` in the client's working directory (`src/run.rs:871-908`), shows output in scrollback only, and passes through no approval or deny list. New: `!cmd` output joins the conversation and the model sees it; `!!cmd` runs privately: shown on every device, kept out of model context and compaction. The owner's own command is not approval-prompted but keeps the sandbox, the deny list, deny rules and the pre_tool veto. Guests cannot send it. Tests: `app/tests/d1.rs::a_bang_line_is_a_shell_action_and_never_a_submit`; `app/tests/shell.rs::double_bang_submits_a_private_shell_action`.
- **Mouse wheel, click cursor, drag-copy, click tool summary (PLAN 10.2a, row 4).** Old TUI: mouse capture; wheel scrolls, click moves the cursor, drag selects and copies in-app, click opens the tool panel. New terminal: no capture; scrolling is PgUp, PgDn, Home and End; selection and copy are the terminal's; Ctrl+T opens the panel. Web: the wheel scrolls and a press on the tool strip toggles the panel (the web host is not a terminal and keeps the gestures).
- **`/issues` (PLAN 10.2a, "Keep /issues?").** Old TUI: `/issues [add, done]` reads `<memory_root>/<slug>/issues.jsonl` on the server through the old in-process UI (src/run.rs:1868). New: not carried over; no arm, no frame. Decided, not a gap.
- **Theme switching and custom theme files (PLAN 10.2a, "Theme switching").** Old TUI: several built-in palettes and themes from a directory (src/theme.rs:44-124, src/lib.rs:889-897). New: Oculus dark, Oculus light and system; `/theme`, the Theme picker and the saved choice (`app/src/theme.rs`). No custom theme files.
- **Model picker save (PLAN 10.2a, row 3).** Old TUI and new app agree (row DONE above): selecting a model changes the session; Shift+S saves the default model and provider (`src/lib.rs:1553-1573`; `app/tests/app3.rs::shift_s_saves_the_highlighted_model_and_provider_as_the_default`). Listed here because the decision names it.
- **Esc on an idle composer (PLAN 10.2a, row 5).** Decided: Esc clears the draft. Old: selection, then live tail, then input. The decision covers the input step; the live-tail step is still a gap (row PARTIAL above).
- **Removing a saved credential (PLAN 10.2a, last row).** Decided: add it. Done: `AuthRemove` in the core and the server, and the Auth row `d` (row DONE above).

Undecided divergences (no PLAN 10.2a entry; status is PARTIAL in the tables, so they are not counted as decisions):

- Enter while a turn runs steers (old: refused with a toast).
- `/settings` opens Settings (old: Auth).
- Alt+Enter queues (old: newline).

## Deliberately not carried over

- Ctrl+O transcript-detail toggle (src/lib.rs:925-928). It changed only the status label (src/lib.rs:3091). Ctrl+O now opens Sessions.
- Alt+Enter as newline. Alt+Enter queues (`app/src/help.rs:204`). Newline is Shift+Enter or Ctrl+J. (Listed as PARTIAL above because no decision is recorded.)
- `/share` publishing only the last answer (src/lib.rs:2655-2666). The shared table defines `/share` as publishing the transcript (`crates/modron-view/src/slash.rs` 185-190).
- `/plan <task>` submitting only a plan-first task. The shared table defines `/plan` as a phase switch (`crates/modron-view/src/slash.rs` 129-134). The app sets the phase, then sends the task.
- The engine running in-process with no server (src/run.rs:443-474). The app is always a client of `modron serve` (`app/README.md`).
- `modron rewind` and the rewind-marker resume. These are CLI features.
- tui-dev hot reload. No such code exists.
- The update notice. The old TUI never drew it.
- The context-meter bar (src/lib.rs:3863-3875). Only tests call it.
- Decided, see "Different by decision": `/issues`, mouse, custom themes.

## In the new app but not the old TUI

Each item was checked against the old TUI's parse table (`src/lib.rs` 240-269), its handler arms (2497-2716), and a search of `crates/modron-tui/src`.

- **Sessions screen.** Ctrl+O or `/sessions`; filter; `n` for a new session; `d` to delete with an inline confirm; j/k; Enter attaches (`app/src/chat.rs` `route_screen_key` 924-949; `app/src/nav.rs` `AskDelete`; `app/src/screens.rs` `sessions_body`). The old `/sessions` arm (src/lib.rs:2553-2556) printed a pointer to the CLI, and the old TUI has no delete.
- **Settings screen** with the rows listed above, opened by F2 (`route_key` 642) or `/settings`. Old `/settings` opened the auth manager (src/lib.rs:262).
- **Plan decision block.** `y` approves; `r` revises with the composer text as the note (`chat.rs` `plan_block` 2184, `PLAN_KEYS` 283; `route_key` 725-731). The old TUI has no plan decision keys; its `/plan` submits a plan-first task (src/lib.rs:2671).
- **Phase and effort.** `/plan` and `/build` switch the phase; `/effort` sets reasoning effort (`slash.rs` 161-170 and 193; `view.rs` `effort_next`). Neither the old parse table (240-269) nor the old handler has `/build` or `/effort` (checked by search of `crates/modron-tui/src/lib.rs`).
- **Queue.** Alt+Enter queues (`chat.rs` `queue_route` 800); Backspace on an empty composer removes the last queued message (`route_key` 691-693); queue chips above the composer (`chat.rs` `queue_chips` 2063). The old TUI refuses Enter while running and has no queue.
- **Steering.** Enter while a turn runs steers it (`client.rs` `send_message_with`, 771-795); steer echoes above the composer (`chat.rs` `steer_rows` 2088).
- **Sub-agent screen.** Ctrl+A opens the first running agent (`route_key` 651-656); Enter steers; `k` on an empty draft asks to kill, with a y/n confirm (`route_screen_key` Agent 877-886; `nav.rs` `AskKill`); `app/src/screens.rs` `agent_panel` (922). The old TUI has sub-agent rows only (src/lib.rs:3329-3380).
- **Confirm prompts** for session delete, agent kill, automation and rule removal, task merge, and credential removal (`nav.rs` `AskDelete`, `AskKill`, `AskRemove`, `AskMerge`, `AskSettingRemove`, `AskAuthRemove`). The old TUI has none of these except its own pending-command lists.
- **Automations screen and tasks screen** (`/automations`, `/tasks`, `/worktree`). The old TUI printed "automations screen is not in this app yet" at HEAD of this app only; the old TUI has no automations screen (`crates/modron-tui/src` search).
- **Reconnect with backoff** (2 s doubling to 30 s, `session.rs` `retry_delay` 302, `RETRY_FIRST` 77, `RETRY_MAX` 79) and re-attach to the same session; Esc or Ctrl+C leaves while retrying (`bin` Esc arm). The old TUI has no reconnect (no `reconnect` or `backoff` in `crates/modron-tui/src`).
- **Built-in demo** (`--demo`, `app/src/startup.rs` 108; `app/src/demo.rs`). The old TUI has none.
- **Share-link session adoption.** A `session` attach parameter adopts the session the first snapshot names (`app/tests/live.rs::a_share_link_adopts_the_session_its_snapshot_names`).
- **F1 and F2.** Help and Settings keys (`route_key` 642-643). The old help was `?` (src/lib.rs:275-412 keys; F1 not re-read).
- **Startup source order and fatal screen.** MODRON_URL or a unix socket, then a probe, then start a server, then a fatal screen with two ways out (`startup.rs` `decide` 266; `bin` `fatal_lines` 186). The old TUI runs in-process.
- **Owner portal (web only).** Sign-in, session list with refresh, invites, sign-out (`app/src/portal.rs`; `nav.rs` Portal and SignIn; `screens.rs` `portal_frame`). Not reachable from the terminal host.
- **Web controls.** Queue, Attach, Stop and chip Dequeue buttons; the tool strip press toggles the panel (`chat.rs` `WEB_CONTROLS` 287, `composer_controls` 2270-2291, `tool_strip` 2110). Web only.
- **Web share settings and invites.** Create, list and revoke viewer and approver invites (`nav.rs` share section; test `app/src/nav.rs::the_share_section_needs_an_owner_and_a_session`; `app/web-test.js` `testInviteFromSettings`). Web only.
- **Permission-rule display and removal.** The rules and allow and deny lists are Settings rows, removable with `d` and a confirm (`nav.rs` `settings_rows` 1530-1606).
- **Effort, theme, persona and Auth rows** in the Settings screen (`nav.rs` `settings_rows`).
- **Not present (checked):** a "wake" feature (the only "wake" is the NSH frame-wake callback, `bin` run loop); a roles settings screen (the ledger's ROLE row is the server's role table, `crates/modron-server/src/dispatch.rs`, not a screen).
- **Committed since PARITY-2 (were uncommitted then):** the automations and tasks screens, `/clear`, note expiry, the macOS clipboard JXA read (`app/src/clipboard.rs` 108-156).
- **Previously not started, now present:** the web attach control (`chat.rs` 2258-2291; `app/web-test.js` `testAttachSendsOneImage`).

## Untested DONE rows

DONE in code, with no test at app level that exercises the behaviour (the behaviour is either untested, or only its key name or mapping is tested). Eleven rows:

1. Left / Right (routing only; caret editing is NSH's).
2. Multi-line Up/Down caret movement (routing only).
3. Delete (key name and routing only).
4. /clear hides the rows (the mapping is tested; the hide is not).
5. /exit (the mapping is tested; the exit path is not).
6. Generative UI blocks in the app (core test only).
7. Turn-end usage line in the app (core only).
8. Swarm summary at turn end in the app (core only).
9. Context 80 percent warning in the app status line (core test only).
10. Protocol handshake refusal (no test of a mismatch).
11. The 10 KiB cap on `!command` output (engine constant; no app test).

## Ledger claims not found in code

Searched in the code (not in the ledger text). Two kinds: claims the code contradicts, and claims the code confirms that the ledger lists as open.

- **REL, FINDING: "`modron upgrade` does not verify the downloaded archive against SHA256SUMS."** Not found; contradicted. `crates/modron-cli/src/upgrade.rs:355` calls `verify_archive` from `checksums.rs`, and the UPGRADE-SUM row (LEDGER) is DONE. The REL finding is stale and should be removed from the ledger.
- **PARITY-D2, "NOT DONE" list.** Attachments in the app, the automations, worktree, allow and deny screens, `/clear`, toast expiry, idle hints, and queue and steer formats "still app-local": all present in code (see the tables above). Stale.
- **APP-6, "the resume line printed on exit remain untested."** A test now exists: `app/src/bin/modron-app-tui.rs::the_resume_line_prints_on_exit_only_for_an_attached_session`. Stale.
- **APP-4/5, "rows lack the ahead/files summary."** Present: `app/src/nav.rs::the_tasks_screen_shows_the_core_rows_and_their_summaries` (2548) and the core `task_rows` (WIRE-2). Superseded by APP-8.
- **APP-7, "the /agent picker has no persona names" and "the credentials screen has no remove control yet."** Both superseded by APP-8 and AUTH-REMOVE: `app/src/nav.rs::the_agent_picker_asks_for_personas_and_lists_their_text_with_the_current_marked` (2521) and `nav.rs::d_on_a_signed_in_credential_asks_then_removes_it` (2591).
- **Spinner "ticks every 120 ms while running and not at all when idle" (APP-7).** Holds for the terminal: the loop schedules a tick only while a turn runs or tasks are open (`modron-app-tui.rs` 346-356; `chat.rs` `tick_deadline` 1862). The host's `tick` still republishes while idle with an empty composer for the hint rotation (`chat.rs` 1418-1428), which the web frame uses.
- **AUTH-REMOVE, "refuses a sign-in that only Codex CLI or Claude Code holds."** Partly confirmed: the Codex CLI wording is in `crates/modron-core/src/auth.rs`. The Claude Code case was not read. Core is being edited by another agent; this is a read only.
- **PersistDefaultModel and AuthRemove "local connections only."** Confirmed by the relay tier table: `crates/modron-server/src/dispatch.rs:354-356` refuses both for tunnel roles, and the test at `dispatch.rs` 3197 covers AuthRemove.
- **APP-8, "a persona cannot be cleared: the server refuses an empty name."** Confirmed: `crates/modron-server/src/hub.rs:1276-1292` refuses any persona name it does not hold, and the empty name is one of them.
- **SHELL-2, "app uses parse_shell_input for `!!` and offers the save control."** Confirmed: `chat.rs` `submit_action` 765-766 and the Models Shift+S path (`chat.rs` 900; `screens.rs` 757-763).
- **SLASH-TABLE, "a test lists every old-TUI command as shared or TUI-only."** Confirmed: `crates/modron-view/src/slash.rs` `every_tui_catalog_command_is_shared_or_tui_only` (487).

Items not checked: "app 190", "web-test.js 27 of 27", "tests 1381", "283 tests", and similar pass counts (nothing was run). C7a's pty lead smoke (a manual run, not checkable from code).

## Not verified

- No cargo run, no test run, no terminal or web run. Statements about key handling come from reading `app/src/bin/modron-app-tui.rs` and `app/src/chat.rs`.
- The e2e and web suites were read, not run. The coverage claims below come from their scenario lists.
- What a real terminal sends for Shift+Enter and whether bracketed paste arrives as one event were not checked (inferred from the crossterm key mapping and the absence of keyboard-enhancement flags).
- The macOS clipboard path was never run on macOS; the Linux clipboard tools were not run.
- The system theme (`app/src/theme.rs`) was read, not run in a terminal or browser.
- Startup time was not measured.

## Answers, from the evidence only

**(1) What a daily terminal user would still lose by switching from the old UI to `modron app` today:**

- A stray y, a or n typed while an approval prompt appears decides it (no guard).
- Alt+Enter queues instead of a newline; a real Shift+Enter probably does not add one in most terminals; Ctrl+J does.
- Enter during a run steers the turn instead of being refused (a change, not a loss).
- No mouse: no wheel scrolling, click-to-open or drag-copy in the app (by decision); PgUp, PgDn, Home and End scroll; the terminal's own selection still works.
- Esc on an idle composer no longer returns the scrollback to the live tail.
- Smaller: no "lines above" count; `/allow` prints a list instead of a menu; `/auth` arguments ignored; `/sandbox bubblewrap` rejected; `--max-cost`, `--max-turns`, `--verbose`, `--attach` rejected; no turn count while running; idle hint wording differs.
- Decided losses: `/issues`, custom themes, and `!command` output now reaches the model.

**(2) Is anything on that list a blocker for deleting `crates/modron-tui`?** Yes, but not because of a feature. `crates/modron-cli` depends on it and calls `modron_tui::run_with` (`main.rs:705`, `769`, `816`), so deleting the crate changes the `modron` command itself, which REL says must stay unchanged. Of the feature gaps, only the approval type-ahead guard is a blocker on safety grounds: without it, a stray key can approve a tool call. The newline keys are the next thing to fix. The rest are not blockers.

**(3) The three riskiest DONE rows** (implemented and tested only against scripted fakes or headless rendering, never exercised by a person in a real terminal or browser):

1. **Esc interrupts a running turn** (Keys). Tests use a scripted host and client (`app/tests/chat.rs::escape_sends_interrupt`); web uses a scripted WebSocket server (`web-test.js` `testEscapeInterrupts`). The e2e scenario S4 stops a run with the browser Stop control, not Esc. The pty scenario T1 presses Esc only on an approval prompt, where it denies.
2. **Image attachments through Ctrl+V** (Input prefixes). The clipboard reads are tested with a fake runner (`app/src/clipboard.rs`); the macOS JXA path never ran on macOS; `app3.rs` stages a fake image instead of reading the clipboard. No e2e scenario covers it.
3. **Ctrl+C aborts a running turn** (Keys). The mid-turn first press is tested as a pure function (`bin::ctrl_c_interrupts_once_mid_turn_and_exits_otherwise`). The pty scenario T1 presses Ctrl+C twice and checks the exit, but does not assert that the first press interrupted a turn.

The e2e suite (`e2e/run.mjs`, not run here) covers these rows: S1, S6a and S6b (attach and replay, web); S2 (submit, web); S3 (approve, web); S4 (stop control, web); S5 (approval resolved on a second viewer); W1 (steer and queue while an approval waits); W2 (`/plan`); W3 (effort and mode pickers, web); W4 (`!cmd`, `!!cmd`, owner shell in deny mode); W5 (sessions refusal over a tunnel); W6 (`/compact`); and T1, the one real-terminal scenario, covering typed submit, approval, Esc deny, Ctrl+C exit, the resume line, and that the owned server stops. The S7, P1 to P4 scenarios cover portal rows, which are not among the 140.
