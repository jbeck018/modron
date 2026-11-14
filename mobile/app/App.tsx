// Modron Remote - the mobile shell over the shared remote-client core
// (`src/lib.rs` bridges to `mobile/rust-core`). The screen renders the
// snapshot's pre-rendered strings verbatim (AGENTS.md parity contract):
// status_line, tool lines, transcript rows, decision labels are the same
// copy the TUI and desktop show - TSX styles them, never re-formats them.
import { mount, signal, computed, Text, View } from "nsh";
import { Input, NavStack, createNavStack } from "nsh/base";
import { Scroll } from "nsh";
import { Remote } from "modron-remote/native";

// ---- snapshot types (mirrors of modron-mobile-core's serde JSON) ----

interface Usage {
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens: number;
}

interface DecisionOption {
  decision: "allow_once" | "allow_all" | "deny";
  label: string;
}

interface PendingApproval {
  request_id: string;
  tool: string;
  args_summary: string;
  arguments_text: string;
  options: DecisionOption[];
}

interface RenderedRow {
  entry: { kind: string };
  lines: string[];
}

interface SessionMeta {
  id: string;
  title: string;
  updated_at: string;
  message_count: number;
}

interface AutomationRow {
  name: string;
  prompt: string;
  interval_minutes: number;
  last_run: string | null;
}

// One row of the server's provider registry (auth status included).
interface ProviderRow {
  id: string;
  label: string;
  status: string;
  oauth_capable: boolean;
}

// Server-side sign-in progress streamed while an oauth flow runs.
interface OAuthState {
  provider: string;
  stage: string;
  message: string;
  url: string | null;
  user_code: string | null;
}

// The router's default-model picture (what is signed in and the
// derived recommendation).
interface RouterDefaultsWire {
  authed: string[];
  recommended_cheap: string | null;
  recommended_strong: string | null;
  cheap_fallback: string[];
  strong_fallback: string[];
}

// One row of the shared slash-command table (modron-view::slash), served
// by Remote.slashCommandsJson() as name/usage/about/scope.
interface SlashCommand {
  name: string;
  usage: string;
  about: string;
  scope: string;
}

interface Snapshot {
  connection_label: string;
  session_id: string | null;
  server_version: string;
  running: boolean;
  running_label: string;
  status_line: string;
  streaming_text: string;
  streaming_reasoning: string;
  show_reasoning: boolean;
  tool_lines: string[];
  turn_summary_line: string | null;
  subagent_rows: string[];
  pending_approval: PendingApproval | null;
  approval_mode_label: string;
  sandbox: string;
  allow_tools: string[];
  deny_tools: string[];
  cumulative_tokens: number;
  session_cache_pct: number | null;
  turns_completed: number;
  tool_calls_total: number;
  model: string;
  provider: string;
  goal: string;
  sessions: SessionMeta[];
  automations: AutomationRow[];
  providers: ProviderRow[];
  oauth: OAuthState | null;
  router_defaults: RouterDefaultsWire | null;
  last_seq: number;
  last_error: string | null;
  last_note: string | null;
  history: RenderedRow[];
}

// ---- app state ----

// Demo mode renders the scripted snapshot with no server; toggle off to
// talk to a real one (ws://127.0.0.1:7699 by default).
const demo = signal(true);
// Once a connect starts, real server state must flow into the snapshot
// even while demo is technically still on (it flips off on success).
const live = signal(false);
const snap = signal<Snapshot>(JSON.parse(Remote.demoSnapshotJson()) as Snapshot);
const url = signal(Remote.defaultUrl());
const draft = signal("");
const connectError = signal<string | null>(null);
const busy = signal(false);
// The /help note (and unknown-command hint), rendered above the composer.
const helpNote = signal<string | null>(null);
// Reasoning streaming is opt-in (AGENTS.md invariant 5; the TUI's
// /reasoning and the desktop's reasoning chip default off). The TSX
// signal gates the render and mirrors into the core via
// Remote.setShowReasoning, whose fold drops deltas while off.
const showReasoning = signal(false);
// The automation add-form fields (settings view).
const autoName = signal("");
const autoPrompt = signal("");
const autoInterval = signal("");
// API-key drafts per provider id (settings providers card).
const keyDrafts = signal<Record<string, string>>({});
// Settings editors: tool lists and the standing goal (drafts overlay
// the snapshot's current values until saved). Null = untouched (falls
// back to the snapshot's value); "" = the user cleared the field, which
// saves the empty value so the server clears it (ConfigPatch carries
// Some([]) / Some("") for exactly that).
const allowDraft = signal<string | null>(null);
const denyDraft = signal<string | null>(null);
const goalDraft = signal<string | null>(null);
// Router defaults card drafts (cheap/strong model ids).
const cheapDraft = signal("");
const strongDraft = signal("");
// New-task title (settings tasks card).
const taskTitle = signal("");
// Sessions-screen delete state: bulk-select mode, the selected ids, and
// the row id whose delete control is armed (two-tap confirm, matching
// the desktop's trash -> check flow).
const editMode = signal(false);
// Approval sheet: the expandable full-call block (parity D7 "view full call").
const approvalDetail = signal(false);
const selectedIds = signal<string[]>([]);
const confirmingId = signal<string | null>(null);

// The shared slash-command table is compile-time (modron-view::slash): fetch
// it once at startup for the composer's suggestions and the /help body.
const slashCommands = signal<SlashCommand[]>([]);
try {
  slashCommands.set(JSON.parse(Remote.slashCommandsJson()) as SlashCommand[]);
} catch {
  // The table lives in the binary; an empty list only hides suggestions.
}

const POLL_MS = 250;
// The raw snapshot JSON, kept so the poll can gate on string equality:
// a tick that receives the same bytes as the last one (the common case
// at 4 polls/sec) re-parses nothing and notifies no consumer. Signals
// already compare with `===` (the SignalOptions.equals default), which
// is the string equality wanted here.
const rawSnap = signal("");
const poll = setInterval(() => {
  if (!live() || demo()) return;
  try {
    const json = Remote.snapshotJson();
    if (json === rawSnap()) return;
    applySnapshotJson(json);
  } catch {
    // between connections the snapshot is `{}`-shaped; keep the old one
  }
}, POLL_MS);

// Parse a fresh snapshot JSON into the render signal. The raw copy is
// the poll's equality gate; keeping both in one helper means the gate
// is never left stale by a direct set.
function applySnapshotJson(json: string): void {
  rawSnap.set(json);
  snap.set(JSON.parse(json) as Snapshot);
}

const stack = createNavStack<"sessions" | "chat" | "settings">(["sessions"]);

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// The handshake + session list run in the background (the Rust client is
// fire-and-forget so the UI thread never blocks, web included): poll the
// snapshot until connected, fire the list, poll for rows, attach the
// first one. Errors surface through last_error.
async function connectAndOpen(): Promise<void> {
  connectError.set(null);
  busy.set(true);
  live.set(true);
  try {
    Remote.connect(url(), null);
    let connected = false;
    for (let i = 0; i < 40; i++) {
      const s = JSON.parse(Remote.snapshotJson()) as Snapshot;
      if (s.connection_label === "connected") {
        connected = true;
        break;
      }
      if (s.last_error) throw new Error(s.last_error);
      await sleep(250);
    }
    if (!connected) throw new Error("connect timed out");

    Remote.listSessionsJson();
    let sessions: SessionMeta[] = [];
    for (let i = 0; i < 20; i++) {
      const s = JSON.parse(Remote.snapshotJson()) as Snapshot;
      sessions = s.sessions;
      if (sessions.length > 0) break;
      if (s.last_error) throw new Error(s.last_error);
      await sleep(250);
    }
    if (sessions.length === 0) throw new Error("no sessions on the server yet");

    Remote.attach(sessions[0].id, 0);
    // A fresh connect built a new client (preferences reset with it):
    // re-apply the reasoning opt-in so the core's fold gate matches.
    try {
      Remote.setShowReasoning(showReasoning());
    } catch {
      // The client exists by now; never happens in practice.
    }
    demo.set(false);
    applySnapshotJson(Remote.snapshotJson());
    stack.push("chat");
  } catch (e) {
    // A failed connect must not leave live on: the poll would keep
    // parsing the `{}`-shaped no-client snapshot over the demo view.
    live.set(false);
    connectError.set(e instanceof Error ? e.message : String(e));
  } finally {
    busy.set(false);
  }
}

async function decide(option: DecisionOption): Promise<void> {
  const approval = snap().pending_approval;
  if (!approval) return;
  if (demo()) {
    // Demo has no server to resolve it: reflect what ApprovalResolved does.
    snap.update((s) => ({ ...s, pending_approval: null }));
    return;
  }
  try {
    Remote.approve(approval.request_id, option.decision);
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

function submitDraft(): void {
  const task = draft().trim();
  if (task.length === 0) return;
  if (task.startsWith("/")) {
    // Slash commands run client-side (AGENTS.md parity: only what this
    // surface can execute; the rest answers "unknown").
    runSlash(task);
    draft.set("");
    return;
  }
  helpNote.set(null);
  if (demo()) {
    draft.set("");
    return;
  }
  try {
    Remote.submit(snap().session_id, task);
    draft.set("");
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// Only what this shell can execute - no invented exports. The set is
// DERIVED from the served table (`slashCommands`, modron-view::slash):
// this shell implements every command the shared table lists, so a
// shared addition flows through with no hand-copied drift, and the
// App.test.tsx drift guard pins the served table to what /help shows.
const EXECUTABLE_SLASH = new Set(
  slashCommands().map((command) => command.name),
);

// The suggestions above the composer: the executable slice of the shared
// table, filtered by the word being typed (hidden once args start).
function slashSuggestions(): SlashCommand[] {
  const text = draft();
  if (!text.startsWith("/") || text.includes(" ")) return [];
  const token = text.slice(1).toLowerCase();
  return slashCommands().filter(
    (command) =>
      EXECUTABLE_SLASH.has(command.name) && command.name.startsWith(token),
  );
}

// Publish the attached session's transcript as a read-only page; the URL
// lands in last_note, which the poll already renders.
function publishTranscript(): void {
  const title =
    snap().sessions.find((s) => s.id === snap().session_id)?.title ??
    "modron transcript";
  const content = snap().history
    .map((row) => row.lines.join("\n"))
    .join("\n\n");
  try {
    Remote.publishPage(title, content);
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// Open the settings view; ask the server for automations once so the rows
// are fresh (the demo snapshot already carries its own).
function enterSettings(): void {
  try {
    Remote.listAutomationsJson();
    Remote.authProvidersJson();
    Remote.routerDefaultsJson();
  } catch {
    // Demo or between connections: render what the snapshot has.
  }
  stack.push("settings");
}

// Start a provider's server-side sign-in; progress streams into the
// snapshot's oauth field (the browser opens on the server machine).
function signIn(provider: string): void {
  if (demo()) {
    connectError.set("sign in needs a live server");
    return;
  }
  try {
    Remote.authStart(provider);
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// Save an API key for a non-oauth provider, then refresh the rows.
function saveKey(provider: string): void {
  const key = (keyDrafts()[provider] ?? "").trim();
  if (!key) return;
  if (demo()) {
    connectError.set("saving keys needs a live server");
    return;
  }
  try {
    Remote.authSetKey(provider, key);
    Remote.authProvidersJson();
    keyDrafts.set({ ...keyDrafts(), [provider]: "" });
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// Apply a config patch to the session (mode, sandbox, tools, goal).
// The server applies patches to future turns only; demo echoes locally
// so the controls stay interactive without a server.
function setConfig(patch: Record<string, unknown>): void {
  if (demo()) {
    const next = { ...snap() };
    // The TSX cannot call Rust here, so this echo mirrors the canonical
    // mapping by hand: modron_mobile_core::approval_mode_label
    // (mobile/rust-core/src/view.rs: ApproveAll -> "yolo", Ask/None ->
    // "ask", Deny -> "deny"). Keep the two in sync.
    if (typeof patch.approval_mode === "string") {
      next.approval_mode_label =
        patch.approval_mode === "approve_all" ? "yolo" : patch.approval_mode;
    }
    if (typeof patch.sandbox === "string") next.sandbox = patch.sandbox;
    if (Array.isArray(patch.allow)) next.allow_tools = patch.allow as string[];
    if (Array.isArray(patch.deny)) next.deny_tools = patch.deny as string[];
    if (typeof patch.goal === "string") next.goal = patch.goal;
    snap.set(next);
    return;
  }
  try {
    Remote.setConfigJson(JSON.stringify(patch));
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// Cycles match the TUI's shift+tab order for approval mode; sandbox
// walks off -> seatbelt -> bwrap.
const APPROVAL_CYCLE: Record<string, string> = {
  ask: "yolo",
  yolo: "deny",
  deny: "ask",
};
const SANDBOX_CYCLE: Record<string, string> = {
  off: "seatbelt",
  seatbelt: "bwrap",
  bwrap: "off",
};

function cycleApproval(): void {
  const label = snap().approval_mode_label;
  setConfig({ approval_mode: APPROVAL_CYCLE[label] ?? "yolo" });
}

function cycleSandbox(): void {
  const label = snap().sandbox;
  setConfig({ sandbox: SANDBOX_CYCLE[label] ?? "off" });
}

function splitList(raw: string): string[] {
  return raw
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

function saveTools(): void {
  // `??` falls back only when untouched (null): a cleared field ("")
  // sends the empty list so the server clears the tools.
  setConfig({
    allow: splitList(allowDraft() ?? snap().allow_tools.join(", ")),
    deny: splitList(denyDraft() ?? snap().deny_tools.join(", ")),
  });
  allowDraft.set(null);
  denyDraft.set(null);
}

function saveGoal(): void {
  // Same null-vs-"" rule: cleared sends the empty goal so the server
  // clears it; untouched re-saves the current goal (a no-op).
  const goal = (goalDraft() ?? snap().goal).trim();
  setConfig({ goal });
  goalDraft.set(null);
}

// Demo gates: fork/tasks/router writes need a live server; the demo
// snapshot carries the display data only.
function needsServer(action: string): boolean {
  if (!demo()) return true;
  connectError.set(`${action} needs a live server`);
  return false;
}

function tryRemote(action: string, f: () => void): void {
  if (!needsServer(action)) return;
  try {
    f();
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

function forkSession(): void {
  tryRemote("fork", () => Remote.forkSession());
}

function requestTaskStatus(): void {
  tryRemote("task status", () => Remote.taskStatus());
}

function mergeTask(): void {
  tryRemote("merge task", () => Remote.mergeTask(null));
}

function createTask(): void {
  const title = taskTitle().trim();
  if (!title) return;
  tryRemote("create task", () => Remote.createTask(title, null));
  taskTitle.set("");
  try {
    if (!demo()) Remote.listSessionsJson();
  } catch {
    // The refreshed list lands with the next poll anyway.
  }
  if (demo()) {
    stack.push("chat");
  }
}

function useRecommended(): void {
  const rec = snap().router_defaults;
  if (!rec) return;
  cheapDraft.set(rec.recommended_cheap ?? "");
  strongDraft.set(rec.recommended_strong ?? "");
}

function saveDefaults(): void {
  const cheap = (cheapDraft() || snap().model).trim();
  const strong = (strongDraft() || snap().model).trim();
  if (!cheap || !strong) return;
  if (demo()) {
    snap.set({
      ...snap(),
      last_note: `router defaults saved: ${cheap} / ${strong}`,
    });
    return;
  }
  try {
    Remote.persistRouter(cheap, strong, null, null);
    cheapDraft.set("");
    strongDraft.set("");
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// Delete one session (two-tap confirm lands here) or every selected id
// from bulk mode. Demo filters the local snapshot; a live server notes
// the outcome and the refreshed list is the truth.
function deleteSessions(ids: string[]): void {
  for (const id of ids) {
    if (demo()) {
      snap.set({
        ...snap(),
        sessions: snap().sessions.filter((s) => s.id !== id),
      });
      continue;
    }
    try {
      Remote.deleteSession(id);
    } catch (e) {
      connectError.set(e instanceof Error ? e.message : String(e));
    }
  }
  try {
    if (!demo()) Remote.listSessionsJson();
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
  confirmingId.set(null);
  selectedIds.set([]);
  editMode.set(false);
}

function runSlash(raw: string): void {
  const spaceAt = raw.indexOf(" ");
  const name = (
    spaceAt === -1 ? raw.slice(1) : raw.slice(1, spaceAt)
  ).toLowerCase();
  const args = spaceAt === -1 ? "" : raw.slice(spaceAt + 1).trim();
  switch (name) {
    case "new":
      try {
        Remote.newSession(null);
        Remote.listSessionsJson();
      } catch (e) {
        connectError.set(e instanceof Error ? e.message : String(e));
      }
      break;
    case "share":
      publishTranscript();
      break;
    case "settings":
      enterSettings();
      break;
    case "help": {
      // The /help body renders the shared copy, executable rows only.
      const lines = slashCommands()
        .filter((command) => EXECUTABLE_SLASH.has(command.name))
        .map((command) => `${command.usage} - ${command.about}`);
      helpNote.set(["slash commands:", ...lines].join("\n"));
      break;
    }
    case "model":
      if (args.length === 0) {
        helpNote.set("use /model <name>");
        break;
      }
      try {
        // Model only: the server follows the provider via the catalog
        // (pinning the current provider here cross-pins foreign models).
        Remote.selectModel(args);
      } catch (e) {
        connectError.set(e instanceof Error ? e.message : String(e));
      }
      break;
    case "fork":
      forkSession();
      break;
    case "tasks":
      // The tasks card (create) lives in settings; status/merge are in
      // the chat header.
      enterSettings();
      break;
    case "mode":
      cycleApproval();
      break;
    case "sandbox":
      cycleSandbox();
      break;
    case "auth":
      // The providers card (sign in / set key) lives in settings.
      enterSettings();
      break;
    case "remote-control":
      if (args.length === 0) {
        helpNote.set("use /remote-control <bridge-url>");
        break;
      }
      tryRemote("remote control", () => Remote.remoteControl(args));
      break;
    default:
      helpNote.set("unknown command - try /help");
  }
}

function addAutomationFromForm(): void {
  const name = autoName().trim();
  const prompt = autoPrompt().trim();
  const parsed = Number.parseInt(autoInterval(), 10);
  if (name.length === 0 || prompt.length === 0) return;
  // An empty field defaults to 60; a parsed interval must be positive
  // (0 or a negative like "-5" is a nonsense schedule and the bridge
  // takes a u32).
  const minutes = Number.isNaN(parsed) ? 60 : parsed;
  if (minutes <= 0) return;
  try {
    Remote.addAutomation(name, prompt, minutes);
    Remote.listAutomationsJson();
    autoName.set("");
    autoPrompt.set("");
    autoInterval.set("");
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

function removeAutomation(name: string): void {
  try {
    Remote.removeAutomation(name);
    Remote.listAutomationsJson();
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

function stopRun(): void {
  const session = snap().session_id;
  if (demo() || !session) return;
  try {
    Remote.interrupt(session);
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// Toggle streamed reasoning (AGENTS.md invariant 5): the TSX signal
// gates the render, the core's fold drops deltas while off.
function toggleReasoning(): void {
  const next = !showReasoning();
  showReasoning.set(next);
  try {
    Remote.setShowReasoning(next);
  } catch {
    // Demo mode has no client; the TSX gate still applies.
  }
}

// Drop-recovery: resume the open session from the last seen seq (the
// server replays exactly what was missed); the poll flips the header
// back to "connected" once the handshake + re-attach settle.
function reconnectSession(): void {
  const session = snap().session_id;
  if (!session || demo()) return;
  try {
    Remote.reconnect(url(), session, null);
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// Close the connection (settings connection card); connect starts a
// fresh one. The transcript stays until the next connect re-replays it.
function disconnect(): void {
  try {
    Remote.disconnect();
    live.set(false);
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

// ---- shared bits ----

const rowClass = (kind: string): string => {
  switch (kind) {
    case "task":
      return "text-accent";
    case "assistant":
    case "user":
      return "text-foreground";
    case "tool_summary":
    case "swarm":
    case "model":
    case "usage":
    case "compaction":
      return "text-muted-foreground";
    default:
      return "text-foreground";
  }
};

function StatusHeader() {
  return (
    <View class="px-4 pt-[env(safe-area-inset-top)] pb-2 border-b border-border bg-background gap-1">
      <View class="flex-row items-center gap-2">
        <Text class="text-foreground font-semibold text-lg">modron</Text>
        <Text class="text-muted-foreground text-sm">
          {snap().connection_label}
        </Text>
        {/* Drop recovery: resume the open session from the last seen seq
            instead of a fresh connect + full replay. */}
        {!demo() &&
        snap().connection_label === "disconnected" &&
        snap().session_id ? (
          <View
            role="button"
            class="rounded-(--radius) bg-accent px-2 py-1 active:opacity-80"
            onPress={() => reconnectSession()}
          >
            <Text class="text-background text-xs">reconnect</Text>
          </View>
        ) : null}
        <View class="flex-1" />
        {snap().running ? (
          <View
            role="button"
            class="rounded-(--radius) bg-destructive px-3 py-1 active:opacity-80"
            onPress={() => stopRun()}
          >
            <Text class="text-background">stop</Text>
          </View>
        ) : (
          <Text class="text-muted-foreground text-sm">{snap().running_label}</Text>
        )}
      </View>
      {/* The shared status line: <model> - <tok> tok - cache <pct>% - <mode> */}
      <Text class="text-muted-foreground text-xs">{snap().status_line}</Text>
      <Text class="text-muted-foreground text-xs">
        {snap().turns_completed} turns - {snap().tool_calls_total} tools - seq{" "}
        {snap().last_seq}
      </Text>
      <View class="flex-row items-center gap-2 pt-1">
        <View
          role="button"
          class="rounded-(--radius) bg-secondary px-2 py-1 active:opacity-80"
          onPress={() => publishTranscript()}
        >
          <Text class="text-secondary-foreground text-xs">share</Text>
        </View>
        <View
          role="button"
          class="rounded-(--radius) bg-secondary px-2 py-1 active:opacity-80"
          onPress={() => toggleReasoning()}
        >
          <Text class="text-secondary-foreground text-xs">
            reasoning {showReasoning() ? "on" : "off"}
          </Text>
        </View>
        <View
          role="button"
          class="rounded-(--radius) bg-secondary px-2 py-1 active:opacity-80"
          onPress={() => forkSession()}
        >
          <Text class="text-secondary-foreground text-xs">fork</Text>
        </View>
        <View
          role="button"
          class="rounded-(--radius) bg-secondary px-2 py-1 active:opacity-80"
          onPress={() => requestTaskStatus()}
        >
          <Text class="text-secondary-foreground text-xs">task status</Text>
        </View>
        <View
          role="button"
          class="rounded-(--radius) bg-secondary px-2 py-1 active:opacity-80"
          onPress={() => mergeTask()}
        >
          <Text class="text-secondary-foreground text-xs">merge task</Text>
        </View>
      </View>
    </View>
  );
}

function ApprovalSheet() {
  // `<If>` re-evaluates as the snapshot changes (components run once);
  // the sheet appears while an approval is pending.
  return (
    <If when={snap().pending_approval}>
      {(pending) => <ApprovalCard approval={pending()} />}
    </If>
  );
}

function ApprovalCard(props: { approval: PendingApproval }) {
  const approval = props.approval;
  return (
    <View class="mx-3 mb-2 rounded-xl border border-border bg-card p-3 gap-2">
      <Text class="text-foreground font-semibold">
        approve {approval.tool}?
      </Text>
      <Text class="text-muted-foreground text-xs">
        {approval.args_summary}
      </Text>
      <View
        role="button"
        class="self-start active:opacity-80"
        onPress={() => approvalDetail.set(!approvalDetail())}
      >
        <Text class="text-primary text-xs">
          {approvalDetail() ? "hide full call" : "view full call"}
        </Text>
      </View>
      <If when={approvalDetail() ? approval.arguments_text : null}>
        {(text) => (
          <Text class="text-muted-foreground text-xs">{text()}</Text>
        )}
      </If>
      <View class="flex-row gap-2">
        {approval.options.map((option) => (
          <View
            role="button"
            class={
              option.decision === "deny"
                ? "flex-1 rounded-(--radius) bg-destructive px-2 py-2 items-center active:opacity-80"
                : "flex-1 rounded-(--radius) bg-primary px-2 py-2 items-center active:opacity-80"
            }
            onPress={() => void decide(option)}
          >
            <Text
              class={
                option.decision === "deny"
                  ? "text-background text-sm"
                  : "text-primary-foreground text-sm"
              }
            >
              {option.label}
            </Text>
          </View>
        ))}
      </View>
    </View>
  );
}

function ChatScreen() {
  // The JSX below reads the suggestions twice (count gate + rows); the
  // computed runs the filter once per draft change and both reads share
  // the memoized list.
  const suggestions = computed(() => slashSuggestions());
  return (
    <View class="flex-1 bg-background">
      <StatusHeader />
      <Scroll class="flex-1 px-4 pt-3 gap-2">
        {snap().history.map((row) => (
          <Text class={`text-sm ${rowClass(row.entry.kind)}`}>
            {row.lines.join("\n")}
          </Text>
        ))}
        {/* The live turn strip (TUI/desktop parity): the collapsed line
            plus the per-event rows while a turn runs. The core clears
            these at turn end, so non-empty means mid-turn. */}
        {snap().turn_summary_line ? (
          <Text class="text-muted-foreground text-xs">
            {snap().turn_summary_line}
          </Text>
        ) : null}
        {snap().tool_lines.map((line) => (
          <Text class="text-muted-foreground text-xs">{line}</Text>
        ))}
        {/* Live swarm rows (the TUI's swarm block, in flight). */}
        {snap().subagent_rows.map((line) => (
          <Text class="text-muted-foreground text-xs">{line}</Text>
        ))}
        {/* Reasoning streams only when opted in (invariant 5): the core
            drops deltas while off, and the render gate mirrors it. */}
        {showReasoning() && snap().streaming_reasoning.length > 0 ? (
          <Text class="text-muted-foreground text-sm italic">
            {snap().streaming_reasoning}
          </Text>
        ) : null}
        {snap().streaming_text.length > 0 ? (
          <Text class="text-foreground text-sm">
            {snap().streaming_text} _
          </Text>
        ) : null}
        {snap().last_note ? (
          <Text class="text-muted-foreground text-xs">{snap().last_note}</Text>
        ) : null}
        {snap().last_error ? (
          <Text class="text-destructive text-xs">{snap().last_error}</Text>
        ) : null}
      </Scroll>
      <ApprovalSheet />
      {helpNote() ? (
        <View class="mx-3 mb-1 rounded-xl border border-border bg-card p-3">
          <Text class="text-muted-foreground text-xs">{helpNote()}</Text>
        </View>
      ) : null}
      {suggestions().length > 0 ? (
        <View class="mx-3 mb-1 gap-1">
          {suggestions().map((command) => (
            <View
              role="button"
              class="flex-row items-center gap-2 rounded-(--radius) border border-border bg-card px-3 py-2 active:opacity-80"
              onPress={() => {
                runSlash("/" + command.name);
                draft.set("");
              }}
            >
              <Text class="text-accent text-sm font-medium">{command.usage}</Text>
              <Text class="flex-1 text-muted-foreground text-xs">
                {command.about}
              </Text>
            </View>
          ))}
        </View>
      ) : null}
      <View
        class="flex-row gap-2 px-3 pb-[env(safe-area-inset-bottom)] pt-2 border-t border-border bg-background items-center"
      >
        <Input
          class="flex-1 rounded-(--radius) border border-border bg-card px-3 py-2 text-foreground"
          value={draft()}
          placeholder={demo() ? "demo mode - connect to type" : "task... /help"}
          returnKeyType="send"
          multiline
          submitBehavior="submit"
          onChange={(e) => draft.set(e.value)}
          onSubmit={() => submitDraft()}
        />
        <View
          role="button"
          class="rounded-(--radius) bg-primary px-4 py-2 active:opacity-80"
          onPress={() => submitDraft()}
        >
          <Text class="text-primary-foreground">send</Text>
        </View>
      </View>
    </View>
  );
}

function SessionsScreen() {
  return (
    <View class="flex-1 bg-background">
      <View class="px-4 pt-[env(safe-area-inset-top)] pb-3 gap-2 border-b border-border">
        <View class="flex-row items-center gap-2">
          <Text class="text-foreground font-semibold text-xl">modron</Text>
          <View class="flex-1" />
          <View
            role="button"
            class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
            onPress={() => {
              editMode.update((e) => !e);
              selectedIds.set([]);
              confirmingId.set(null);
            }}
          >
            <Text class="text-secondary-foreground text-sm">
              {editMode() ? "done" : "edit"}
            </Text>
          </View>
          <View
            role="button"
            class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
            onPress={() => {
              demo.update((d) => !d);
              if (demo()) {
                snap.set(JSON.parse(Remote.demoSnapshotJson()) as Snapshot);
              } else if (!live()) {
                // Not connected (or the last connect failed): there is
                // no live snapshot to fall back to - re-arm demo so the
                // screen keeps working. (The demo snapshot always sets
                // session_id, so that field cannot tell the two apart.)
                demo.set(true);
              } else {
                // Back to the live snapshot: reset the poll's equality
                // gate so the next tick re-parses even if the server
                // bytes have not changed since demo took over.
                rawSnap.set("");
              }
            }}
          >
            <Text class="text-secondary-foreground text-sm">
              {demo() ? "demo on" : "demo off"}
            </Text>
          </View>
          <View
            role="button"
            class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
            onPress={() => enterSettings()}
          >
            <Text class="text-secondary-foreground text-sm">settings</Text>
          </View>
        </View>
        <View class="flex-row gap-2 items-center">
          <Input
            class="flex-1 rounded-(--radius) border border-border bg-card px-3 py-2 text-foreground"
            value={url()}
            placeholder="ws://127.0.0.1:7699"
            keyboardType="url"
            autoCorrect={false}
            autoCapitalize="none"
            onChange={(e) => url.set(e.value)}
            onSubmit={() => void connectAndOpen()}
          />
          <View
            role="button"
            class="rounded-(--radius) bg-primary px-4 py-2 active:opacity-80"
            onPress={() => void connectAndOpen()}
          >
            <Text class="text-primary-foreground">
              {busy() ? "..." : "connect"}
            </Text>
          </View>
        </View>
        {editMode() ? (
          <View class="flex-row items-center gap-2">
            <Text class="text-muted-foreground text-xs">
              {selectedIds().length} selected
            </Text>
            <View class="flex-1" />
            <View
              role="button"
              class={
                "rounded-(--radius) px-3 py-1 active:opacity-80 " +
                (selectedIds().length
                  ? "bg-destructive"
                  : "bg-secondary opacity-50")
              }
              onPress={() => {
                if (selectedIds().length) deleteSessions(selectedIds());
              }}
            >
              <Text
                class={
                  selectedIds().length
                    ? "text-background text-sm"
                    : "text-secondary-foreground text-sm"
                }
              >
                delete selected
              </Text>
            </View>
            <View
              role="button"
              class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
              onPress={() => {
                editMode.set(false);
                selectedIds.set([]);
              }}
            >
              <Text class="text-secondary-foreground text-sm">cancel</Text>
            </View>
          </View>
        ) : null}
        {connectError() ? (
          <Text class="text-destructive text-xs">{connectError()}</Text>
        ) : null}
      </View>
      <Scroll class="flex-1 px-4 pt-2 gap-2">
        <Text class="text-muted-foreground text-xs">
          {editMode()
            ? "sessions - tap to select"
            : "sessions - tap one to attach"}
        </Text>
        {snap().sessions.map((session) => {
          const selected = selectedIds().includes(session.id);
          const armed = confirmingId() === session.id;
          return (
            <View class="flex-row items-stretch gap-2">
              <View
                role="button"
                class={
                  "flex-1 rounded-xl border bg-card px-3 py-3 gap-1 active:opacity-80 " +
                  (selected && editMode() ? "border-accent" : "border-border")
                }
                onPress={() => {
                  confirmingId.set(null);
                  if (editMode()) {
                    selectedIds.update((ids) =>
                      selected
                        ? ids.filter((id) => id !== session.id)
                        : [...ids, session.id],
                    );
                    return;
                  }
                  if (demo()) {
                    stack.push("chat");
                    return;
                  }
                  try {
                    Remote.attach(session.id, 0);
                    stack.push("chat");
                  } catch (e) {
                    connectError.set(e instanceof Error ? e.message : String(e));
                  }
                }}
              >
                <View class="flex-row items-center gap-2">
                  {editMode() ? (
                    <Text class="text-accent">{selected ? "[x]" : "[ ]"}</Text>
                  ) : null}
                  <Text class="text-foreground font-medium">
                    {session.title}
                  </Text>
                </View>
                <Text class="text-muted-foreground text-xs">
                  {session.message_count} messages - {session.updated_at}
                </Text>
              </View>
              {!editMode() ? (
                <View
                  role="button"
                  class="justify-center px-1 active:opacity-80"
                  onPress={() => {
                    if (armed) {
                      deleteSessions([session.id]);
                    } else {
                      confirmingId.set(session.id);
                    }
                  }}
                >
                  <View
                    class={
                      armed
                        ? "rounded-(--radius) bg-destructive px-3 py-1"
                        : "rounded-(--radius) bg-secondary px-3 py-1"
                    }
                  >
                    <Text
                      class={
                        armed
                          ? "text-background text-sm"
                          : "text-secondary-foreground text-sm"
                      }
                    >
                      {armed ? "confirm?" : "delete"}
                    </Text>
                  </View>
                </View>
              ) : null}
            </View>
          );
        })}
        {snap().sessions.length === 0 ? (
          <Text class="text-muted-foreground text-sm">
            {demo()
              ? "demo transcript ready - toggle demo off and connect to a server"
              : "no sessions yet"}
          </Text>
        ) : null}
      </Scroll>
    </View>
  );
}

function SettingsScreen() {
  return (
    <View class="flex-1 bg-background">
      <View class="flex-row items-center gap-2 px-4 pt-[env(safe-area-inset-top)] pb-3 border-b border-border">
        <View
          role="button"
          class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
          onPress={() => stack.pop()}
        >
          <Text class="text-secondary-foreground text-sm">back</Text>
        </View>
        <Text class="text-foreground font-semibold text-xl">settings</Text>
      </View>
      <Scroll class="flex-1 px-4 pt-3 gap-3 pb-[env(safe-area-inset-bottom)]">
        {/* Connection: the same affordance the sessions screen has. */}
        <View class="rounded-xl border border-border bg-card p-3 gap-2">
          <Text class="text-muted-foreground text-xs">connection</Text>
          <View class="flex-row gap-2 items-center">
            <Input
              class="flex-1 rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
              value={url()}
              placeholder="ws://127.0.0.1:7699"
              keyboardType="url"
              autoCorrect={false}
              autoCapitalize="none"
              onChange={(e) => url.set(e.value)}
              onSubmit={() => void connectAndOpen()}
            />
            <View
              role="button"
              class="rounded-(--radius) bg-primary px-4 py-2 active:opacity-80"
              onPress={() => void connectAndOpen()}
            >
              <Text class="text-primary-foreground">
                {busy() ? "..." : "connect"}
              </Text>
            </View>
          </View>
          <View class="flex-row gap-2 items-center">
            <Text class="text-muted-foreground text-xs flex-1">
              {snap().connection_label}
            </Text>
            {snap().connection_label === "connected" && !demo() ? (
              <View
                role="button"
                class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
                onPress={() => disconnect()}
              >
                <Text class="text-secondary-foreground text-xs">
                  disconnect
                </Text>
              </View>
            ) : null}
          </View>
          {connectError() ? (
            <Text class="text-destructive text-xs">{connectError()}</Text>
          ) : null}
        </View>
        {/* Session config: model/provider read-only, the rest editable
            (the same patches the TUI's /mode, /sandbox, /goal and the
            desktop settings page send). */}
        <View class="rounded-xl border border-border bg-card p-3 gap-2">
          <Text class="text-muted-foreground text-xs">session</Text>
          <Text class="text-foreground text-sm">model: {snap().model}</Text>
          <Text class="text-foreground text-sm">provider: {snap().provider}</Text>
          <View class="flex-row items-center gap-2">
            <Text class="text-foreground text-sm flex-1">
              approval mode: {snap().approval_mode_label}
            </Text>
            <View
              role="button"
              class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
              onPress={() => cycleApproval()}
            >
              <Text class="text-secondary-foreground text-sm">cycle mode</Text>
            </View>
          </View>
          <View class="flex-row items-center gap-2">
            <Text class="text-foreground text-sm flex-1">
              sandbox: {snap().sandbox}
            </Text>
            <View
              role="button"
              class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
              onPress={() => cycleSandbox()}
            >
              <Text class="text-secondary-foreground text-sm">
                cycle sandbox
              </Text>
            </View>
          </View>
          <Input
            class="rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
            value={allowDraft() ?? snap().allow_tools.join(", ")}
            placeholder="allowed tools (comma separated)"
            autoCorrect={false}
            autoCapitalize="none"
            onChange={(e) => allowDraft.set(e.value)}
            onSubmit={() => saveTools()}
          />
          <Input
            class="rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
            value={denyDraft() ?? snap().deny_tools.join(", ")}
            placeholder="blocked tools (comma separated)"
            autoCorrect={false}
            autoCapitalize="none"
            onChange={(e) => denyDraft.set(e.value)}
            onSubmit={() => saveTools()}
          />
          <View
            role="button"
            class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
            onPress={() => saveTools()}
          >
            <Text class="text-secondary-foreground text-sm">save tools</Text>
          </View>
          <Input
            class="rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
            value={goalDraft() ?? snap().goal}
            placeholder="standing goal"
            autoCorrect={false}
            autoCapitalize="none"
            onChange={(e) => goalDraft.set(e.value)}
            onSubmit={() => saveGoal()}
          />
          <View
            role="button"
            class="rounded-(--radius) bg-primary px-3 py-1 active:opacity-80"
            onPress={() => saveGoal()}
          >
            <Text class="text-primary-foreground text-sm">save goal</Text>
          </View>
          <Text class="text-foreground text-sm">
            server: {snap().server_version}
          </Text>
          <Text class="text-foreground text-sm">seq: {snap().last_seq}</Text>
        </View>
        {/* Router defaults: the same recommendation + save flow the
            desktop settings page has (shared microcopy). */}
        <View class="rounded-xl border border-border bg-card p-3 gap-2">
          <Text class="text-muted-foreground text-xs">router defaults</Text>
          {snap().router_defaults ? (
            <View class="gap-1">
              <Text class="text-muted-foreground text-xs">
                signed in:{" "}
                {snap().router_defaults!.authed.join(", ") || "none"}
              </Text>
              <Text class="text-accent text-xs">
                recommended:{" "}
                {snap().router_defaults!.recommended_cheap ?? "?"} (cheap) /{" "}
                {snap().router_defaults!.recommended_strong ?? "?"} (strong)
              </Text>
            </View>
          ) : (
            <Text class="text-muted-foreground text-xs">
              connect to a server for recommendations
            </Text>
          )}
          <Input
            class="rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
            value={cheapDraft()}
            placeholder={`cheap model (default: ${snap().model})`}
            autoCorrect={false}
            autoCapitalize="none"
            onChange={(e) => cheapDraft.set(e.value)}
          />
          <Input
            class="rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
            value={strongDraft()}
            placeholder={`strong model (default: ${snap().model})`}
            autoCorrect={false}
            autoCapitalize="none"
            onChange={(e) => strongDraft.set(e.value)}
          />
          <View class="flex-row gap-2 items-center justify-end">
            <View
              role="button"
              class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
              onPress={() => useRecommended()}
            >
              <Text class="text-secondary-foreground text-sm">
                use recommended
              </Text>
            </View>
            <View
              role="button"
              class="rounded-(--radius) bg-primary px-3 py-1 active:opacity-80"
              onPress={() => saveDefaults()}
            >
              <Text class="text-primary-foreground text-sm">
                save as default
              </Text>
            </View>
          </View>
        </View>
        {/* Tasks: create a session bound to its own worktree/branch;
            status and merge live in the chat header. */}
        <View class="rounded-xl border border-border bg-card p-3 gap-2">
          <Text class="text-muted-foreground text-xs">tasks</Text>
          <View class="flex-row gap-2 items-center">
            <Input
              class="flex-1 rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
              value={taskTitle()}
              placeholder="new task title"
              autoCorrect={false}
              autoCapitalize="none"
              onChange={(e) => taskTitle.set(e.value)}
              onSubmit={() => createTask()}
            />
            <View
              role="button"
              class="rounded-(--radius) bg-primary px-3 py-2 active:opacity-80"
              onPress={() => createTask()}
            >
              <Text class="text-primary-foreground text-sm">create</Text>
            </View>
          </View>
        </View>
        {/* Providers: auth status per registry row; oauth rows get a
            sign-in button (server-side flow, progress streams below),
            api-key rows get a key input. */}
        <View class="rounded-xl border border-border bg-card p-3 gap-2">
          <Text class="text-muted-foreground text-xs">providers</Text>
          {snap().oauth ? (
            <View class="gap-1">
              <Text class="text-accent text-sm">
                {snap().oauth!.provider} sign-in: {snap().oauth!.stage}
              </Text>
              <Text class="text-muted-foreground text-xs">
                {snap().oauth!.message}
              </Text>
              {snap().oauth!.user_code ? (
                <Text class="text-foreground text-sm">
                  code: {snap().oauth!.user_code}
                </Text>
              ) : null}
            </View>
          ) : null}
          {snap().providers.map((provider) => (
            <View class="gap-1">
              <View class="flex-row items-center gap-2">
                <Text class="text-foreground text-sm flex-1">
                  {provider.label}
                </Text>
                <Text class="text-muted-foreground text-xs">
                  {provider.status}
                </Text>
              </View>
              {provider.oauth_capable ? (
                <View
                  role="button"
                  class="rounded-(--radius) bg-secondary px-3 py-1 active:opacity-80"
                  onPress={() => signIn(provider.id)}
                >
                  <Text class="text-secondary-foreground text-sm">
                    sign in
                  </Text>
                </View>
              ) : (
                <View class="flex-row gap-2 items-center">
                  <Input
                    class="flex-1 rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
                    value={keyDrafts()[provider.id] ?? ""}
                    placeholder={`${provider.label} api key`}
                    autoCorrect={false}
                    autoCapitalize="none"
                    onChange={(e) =>
                      keyDrafts.set({
                        ...keyDrafts(),
                        [provider.id]: e.value,
                      })
                    }
                    onSubmit={() => saveKey(provider.id)}
                  />
                  <View
                    role="button"
                    class="rounded-(--radius) bg-primary px-3 py-2 active:opacity-80"
                    onPress={() => saveKey(provider.id)}
                  >
                    <Text class="text-primary-foreground text-sm">save</Text>
                  </View>
                </View>
              )}
            </View>
          ))}
          {snap().providers.length === 0 ? (
            <Text class="text-muted-foreground text-xs">
              connect to a server to load providers
            </Text>
          ) : null}
        </View>
        {/* Automations: the snapshot's rows, removable; the add form
            refreshes the list through the bridge. */}
        <View class="rounded-xl border border-border bg-card p-3 gap-2">
          <Text class="text-muted-foreground text-xs">automations</Text>
          {snap().automations.map((automation) => (
            <View class="flex-row items-center gap-2">
              <View class="flex-1 gap-1">
                <Text class="text-foreground text-sm">{automation.name}</Text>
                <Text class="text-muted-foreground text-xs">
                  every {automation.interval_minutes}m
                </Text>
              </View>
              <View
                role="button"
                class="rounded-(--radius) bg-destructive px-3 py-1 active:opacity-80"
                onPress={() => removeAutomation(automation.name)}
              >
                <Text class="text-background text-sm">remove</Text>
              </View>
            </View>
          ))}
          {snap().automations.length === 0 ? (
            <Text class="text-muted-foreground text-xs">none yet</Text>
          ) : null}
          <View class="gap-1">
            <Input
              class="rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
              value={autoName()}
              placeholder="name"
              autoCorrect={false}
              autoCapitalize="none"
              onChange={(e) => autoName.set(e.value)}
            />
            <Input
              class="rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
              value={autoPrompt()}
              placeholder="prompt"
              onChange={(e) => autoPrompt.set(e.value)}
            />
            <View class="flex-row gap-2 items-center">
              <Input
                class="w-20 rounded-(--radius) border border-border bg-background px-3 py-2 text-foreground"
                value={autoInterval()}
                placeholder="60"
                keyboardType="number-pad"
                autoCorrect={false}
                onChange={(e) => autoInterval.set(e.value)}
              />
              <View
                role="button"
                class="rounded-(--radius) bg-primary px-4 py-2 active:opacity-80"
                onPress={() => addAutomationFromForm()}
              >
                <Text class="text-primary-foreground">add</Text>
              </View>
              <Text class="flex-1 text-muted-foreground text-xs">
                minutes between runs
              </Text>
            </View>
          </View>
        </View>
        {/* Share: publishes the transcript; the URL lands in last_note. */}
        <View class="rounded-xl border border-border bg-card p-3 gap-2">
          <Text class="text-muted-foreground text-xs">share</Text>
          <View
            role="button"
            class="rounded-(--radius) bg-primary px-4 py-2 items-center active:opacity-80"
            onPress={() => publishTranscript()}
          >
            <Text class="text-primary-foreground">publish page</Text>
          </View>
          <Text class="text-muted-foreground text-xs">
            publishes this transcript as a read-only web page
          </Text>
        </View>
        {/* About. */}
        <View class="rounded-xl border border-border bg-card p-3 gap-1">
          <Text class="text-muted-foreground text-xs">about</Text>
          <Text class="text-foreground font-semibold text-lg">modron</Text>
          <Text class="text-muted-foreground text-xs">
            one shared shell over the modron server - web, iOS, desktop, TUI
          </Text>
          <Text class="text-muted-foreground text-xs">
            server {snap().server_version}
          </Text>
        </View>
      </Scroll>
    </View>
  );
}

function App() {
  return (
    <NavStack state={stack}>
      {(route) => (
        <View class="flex-1">
          {route === "sessions" ? (
            <SessionsScreen />
          ) : route === "chat" ? (
            <ChatScreen />
          ) : (
            <SettingsScreen />
          )}
        </View>
      )}
    </NavStack>
  );
}

mount(App);
