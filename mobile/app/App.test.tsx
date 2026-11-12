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
  // Approval sheet: the three shared decision labels.
  expect(texts).toContain("allow once");
  expect(texts).toContain("allow for this run");
  expect(texts).toContain("deny");
  // The stop control (running).
  expect(texts).toContain("stop");
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

