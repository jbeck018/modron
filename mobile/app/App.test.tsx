// `nsh test` runs this file in a headless runner it builds with the crate.
// The app boots in demo mode on the sessions screen; the chat screen
// renders from the scripted snapshot the Rust module hands over (no
// server needed).
import { expect, open, test } from "nsh/test";

function openDemoChat(): Promise<TestApp> {
  return open("./App.tsx").then(async (app) => {
    await app.press("migrate the router to the new hub");
    return app;
  });
}

// The type nsh/test's open() returns; spelled out so the helper stays
// simple above.
type TestApp = Awaited<ReturnType<typeof open>>;

test("demo sessions screen renders", async () => {
  const app = await open("./App.tsx");
  const texts = app.texts();
  expect(texts).toContain("modron");
  expect(texts).toContain("demo on");
  expect(texts).toContain("migrate the router to the new hub");
});

test("demo chat screen renders the shared copy", async () => {
  const app = await openDemoChat();
  const texts = app.texts();
  // Status header (snapshot.status_line verbatim from modron-mobile-core).
  expect(texts).toContain(
    "claude-opus-5 \u00b7 18.4k tok \u00b7 cache 71% \u00b7 ask",
  );
  // Transcript rows (snapshot.history[].lines verbatim).
  expect(texts).toContain("\u276f migrate the router to the new hub");
  expect(texts).toContain("\u23fa 4 tools \u00b7 read \u00d72 \u00b7 glob \u00b7 bash");
  expect(texts).toContain("5900 tok \u00b7 cache 80%");
  // Streaming buffer with its caret.
  expect(texts).toContain("Migrating the router now;  _");
  // The live turn strip while the demo run is in flight.
  expect(texts).toContain("\u25cf bash \u2026");
  expect(texts).toContain("\u23fa 4 tools \u00b7 read \u00d72 \u00b7 glob \u00b7 bash");
  // Approval sheet: the three shared decision labels.
  expect(texts).toContain("allow once");
  expect(texts).toContain("allow all");
  expect(texts).toContain("deny");
  // The stop control (running).
  expect(texts).toContain("stop");
});

// Reasoning is opt-in (parity invariant 5): the chip starts off and the
// toggle flips it (demo carries no reasoning buffer, so only the chip
// state changes on screen).
test("reasoning chip toggles", async () => {
  const app = await openDemoChat();
  expect(app.texts()).toContain("reasoning off");
  await app.press("reasoning off");
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(app.texts()).toContain("reasoning on");
  expect(app.texts()).not.toContain("reasoning off");
});

test("demo approval resolves locally", async () => {
  const app = await openDemoChat();
  expect(app.texts()).toContain("approve edit?");
  await app.press("allow once");
  // Let the reactive update flush before re-reading the screen.
  await new Promise((resolve) => setTimeout(resolve, 200));
  expect(app.texts()).not.toContain("approve edit?");
});

test("composer and stop control exist on the chat screen", async () => {
  const app = await openDemoChat();
  const texts = app.texts();
  expect(texts).toContain("send");
});

// Two-tap delete (parity with the desktop's trash -> check flow): the
// first tap arms the row, the second removes it from the demo snapshot.
test("sessions screen arms and confirms a delete", async () => {
  const app = await open("./App.tsx");
  await app.press("delete >> nth=0");
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(app.texts()).toContain("confirm?");
  await app.press("confirm?");
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(app.texts()).not.toContain("migrate the router to the new hub");
  expect(app.texts()).toContain("audit the tools crate");
});

// Bulk mode: edit toggles selection markers, the action bar deletes the
// selected ids.
test("edit mode selects and bulk deletes", async () => {
  const app = await open("./App.tsx");
  await app.press("edit");
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(app.texts()).toContain("sessions - tap to select");
  await app.press("audit the tools crate");
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(app.texts()).toContain("[x]");
  expect(app.texts()).toContain("1 selected");
  await app.press("delete selected");
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(app.texts()).not.toContain("audit the tools crate");
  expect(app.texts()).toContain("migrate the router to the new hub");
});

// Settings providers card (parity P2): registry rows with auth status,
// sign-in for oauth-capable providers, key entry for the rest.
test("settings renders the providers card", async () => {
  const app = await open("./App.tsx");
  await app.press("settings");
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(app.texts()).toContain("providers");
  expect(app.texts()).toContain("Anthropic (Claude)");
  expect(app.texts()).toContain("subscription (oauth)");
  expect(app.texts()).toContain("sign in");
  expect(app.texts()).toContain("save");
});

const settle = () => new Promise((resolve) => setTimeout(resolve, 100));

// Editable session config (parity P3): the cycles walk the same orders
// the TUI uses, and demo echoes the patch locally. The demo echo only
// accepts the server's WIRE names (approve_all, bubblewrap), so a label
// changing here proves the right wire value was sent.
test("settings cycles approval mode and sandbox", async () => {
  const app = await open("./App.tsx");
  await app.press("settings");
  await settle();
  expect(app.texts()).toContain("approval mode: ask");
  expect(app.texts()).toContain("sandbox: off");
  await app.press("cycle mode");
  await settle();
  expect(app.texts()).toContain("approval mode: yolo");
  await app.press("cycle sandbox");
  await settle();
  expect(app.texts()).toContain("sandbox: seatbelt");
  expect(app.texts()).toContain("save tools");
  expect(app.texts()).toContain("save goal");
});

// Approval cycle: ask -> approve_all (label yolo) -> deny -> ask. Each
// step must land, which pins approve_all as the wire value for yolo.
test("approval cycle sends wire names and wraps", async () => {
  const app = await open("./App.tsx");
  await app.press("settings");
  await settle();
  await app.press("cycle mode");
  await settle();
  expect(app.texts()).toContain("approval mode: yolo");
  await app.press("cycle mode");
  await settle();
  expect(app.texts()).toContain("approval mode: deny");
  await app.press("cycle mode");
  await settle();
  expect(app.texts()).toContain("approval mode: ask");
});

// Sandbox cycle: off -> seatbelt -> bubblewrap (label bwrap) -> off.
test("sandbox cycle sends wire names and wraps", async () => {
  const app = await open("./App.tsx");
  await app.press("settings");
  await settle();
  await app.press("cycle sandbox");
  await settle();
  expect(app.texts()).toContain("sandbox: seatbelt");
  await app.press("cycle sandbox");
  await settle();
  expect(app.texts()).toContain("sandbox: bwrap");
  await app.press("cycle sandbox");
  await settle();
  expect(app.texts()).toContain("sandbox: off");
});

// Router defaults + tasks cards (parity P4): the desktop's microcopy
// verbatim, recommendations from the snapshot.
test("settings shows router defaults and tasks cards", async () => {
  const app = await open("./App.tsx");
  await app.press("settings");
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(app.texts()).toContain("router defaults");
  expect(app.texts()).toContain(
    "recommended: gpt-5.6-terra (cheap) / claude-opus-5-5 (strong)",
  );
  expect(app.texts()).toContain("use recommended");
  expect(app.texts()).toContain("save as default");
  expect(app.texts()).toContain("tasks");
});

// Chat header carries the fork/task controls (parity P4).
test("chat header has share, fork and task controls", async () => {
  const app = await openDemoChat();
  const texts = app.texts();
  expect(texts).toContain("share");
  expect(texts).toContain("fork");
  expect(texts).toContain("task status");
  expect(texts).toContain("merge task");
});

// Drift guard: EXECUTABLE_SLASH is derived from the served table
// (modron-view::slash via Remote.slashCommandsJson), and /help renders
// exactly that table. The names below are the shared table's contract -
// if the served table and what the shell lists ever disagree, this
// fails either way.
test("slash help lists exactly the served table", async () => {
  const app = await openDemoChat();
  await app.type("demo mode - connect to type", "/help");
  await app.press("send");
  const note = app.texts().find((t) => t.startsWith("slash commands:"));
  expect(note).toBeDefined();
  const listed = (note as string)
    .split("\n")
    .slice(1)
    .map((line) => line.split(" ")[0])
    .sort();
  const served = [
    "model",
    "mode",
    "sandbox",
    "new",
    "fork",
    "tasks",
    "share",
    "remote-control",
    "auth",
    "settings",
    "help",
  ].map((name) => "/" + name);
  expect(listed).toEqual([...served].sort());
});




