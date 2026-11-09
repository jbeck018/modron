// Modron Remote - the mobile shell over the shared remote-client core
// (`src/lib.rs` bridges to `mobile/rust-core`). The screen renders the
// snapshot's pre-rendered strings verbatim (AGENTS.md parity contract):
// status_line, tool lines, transcript rows, decision labels are the same
// copy the TUI and desktop show - TSX styles them, never re-formats them.
import { mount, signal, Text, View } from "nsh";
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

interface Snapshot {
  connection_label: string;
  session_id: string | null;
  server_version: string;
  running: boolean;
  running_label: string;
  status_line: string;
  streaming_text: string;
  streaming_reasoning: string;
  tool_lines: string[];
  turn_summary_line: string | null;
  subagent_rows: string[];
  pending_approval: PendingApproval | null;
  approval_mode_label: string;
  cumulative_tokens: number;
  session_cache_pct: number | null;
  turns_completed: number;
  tool_calls_total: number;
  model: string;
  provider: string;
  goal: string;
  sessions: SessionMeta[];
  last_seq: number;
  last_error: string | null;
  last_note: string | null;
  history: RenderedRow[];
}

// ---- app state ----

// Demo mode renders the scripted snapshot with no server; toggle off to
// talk to a real one (ws://127.0.0.1:7699 by default).
const demo = signal(true);
const snap = signal<Snapshot>(JSON.parse(Remote.demoSnapshotJson()) as Snapshot);
const url = signal(Remote.defaultUrl());
const draft = signal("");
const connectError = signal<string | null>(null);
const busy = signal(false);

const POLL_MS = 250;
const poll = setInterval(() => {
  if (demo()) return;
  try {
    // Drain incremental rows too (kept for large transcripts; the full
    // history re-render below is fine at chat scale).
    Remote.takeNewEntriesJson();
    snap.set(JSON.parse(Remote.snapshotJson()) as Snapshot);
  } catch {
    // between connections the snapshot is `{}`-shaped; keep the old one
  }
}, POLL_MS);

const stack = createNavStack<"sessions" | "chat">(["sessions"]);

async function connectAndOpen(): Promise<void> {
  connectError.set(null);
  busy.set(true);
  try {
    Remote.connect(url(), null);
    const sessions = JSON.parse(Remote.listSessionsJson()) as SessionMeta[];
    if (sessions.length === 0) throw new Error("no sessions on the server yet");
    Remote.attach(sessions[0].id, 0);
    demo.set(false);
    snap.set(JSON.parse(Remote.snapshotJson()) as Snapshot);
    stack.push("chat");
  } catch (e) {
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

function stopRun(): void {
  const session = snap().session_id;
  if (demo() || !session) return;
  try {
    Remote.interrupt(session);
  } catch (e) {
    connectError.set(e instanceof Error ? e.message : String(e));
  }
}

function backOut(): void {
  // Parity: Esc backs out one screen.
  stack.pop();
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
      <Text class="text-muted-foreground text-xs line-clamp-2">
        {approval.args_summary}
      </Text>
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
  return (
    <View class="flex-1 bg-background">
      <StatusHeader />
      <Scroll class="flex-1 px-4 pt-3 gap-2">
        {snap().history.map((row) => (
          <Text class={`text-sm ${rowClass(row.entry.kind)}`}>
            {row.lines.join("\n")}
          </Text>
        ))}
        {snap().streaming_reasoning.length > 0 ? (
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
      <View
        class="flex-row gap-2 px-3 pb-[env(safe-area-inset-bottom)] pt-2 border-t border-border bg-background items-center"
      >
        <Input
          class="flex-1 rounded-(--radius) border border-border bg-card px-3 py-2 text-foreground"
          value={draft()}
          placeholder={demo() ? "demo mode - connect to type" : "task..."}
          returnKeyType="send"
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
              demo.update((d) => !d);
              if (demo()) {
                snap.set(JSON.parse(Remote.demoSnapshotJson()) as Snapshot);
              } else if (!snap().session_id) {
                // Not connected yet: keep rendering the demo snapshot.
                demo.set(true);
              }
            }}
          >
            <Text class="text-secondary-foreground text-sm">
              {demo() ? "demo on" : "demo off"}
            </Text>
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
        {connectError() ? (
          <Text class="text-destructive text-xs">{connectError()}</Text>
        ) : null}
      </View>
      <Scroll class="flex-1 px-4 pt-2 gap-2">
        <Text class="text-muted-foreground text-xs">
          sessions - tap one to attach
        </Text>
        {snap().sessions.map((session) => (
          <View
            role="button"
            class="rounded-xl border border-border bg-card px-3 py-3 gap-1 active:opacity-80"
            onPress={() => {
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
            <Text class="text-foreground font-medium">{session.title}</Text>
            <Text class="text-muted-foreground text-xs">
              {session.message_count} messages - {session.updated_at}
            </Text>
          </View>
        ))}
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

function App() {
  return (
    <NavStack state={stack}>
      {(route) => (
        <View class="flex-1">
          {route === "sessions" ? <SessionsScreen /> : <ChatScreen />}
        </View>
      )}
    </NavStack>
  );
}

mount(App);
