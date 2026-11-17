// Screenshot tool for the web target (Program UX-1). Drives the scripted site
// from web-test.js through the top-level screens, writes one PNG per screen
// and variant, and reports two layout facts per shot: horizontal page scroll
// and role=button elements shorter than 44 px.
//
//   node shots.mjs <outDir> [variant ...]
//
// Variants (default: all four): iphone-light, iphone-dark (Playwright's
// devices['iPhone 13'], deviceScaleFactor as the device has it), desktop-light
// and desktop-dark (1280 x 800). Needs the bundle built first:
//   nsh package --target web -o dist < /dev/null
// Uses the preinstalled Playwright (headless Chromium), like web-test.js.
import fs from 'fs';
import path from 'path';
import { createRequire } from 'module';

const require = createRequire(import.meta.url);
const { chromium, devices } = require('playwright');
const T = require('./web-test.js');

const VARIANTS = {
  'iphone-light': { ...devices['iPhone 13'], colorScheme: 'light' },
  'iphone-dark': { ...devices['iPhone 13'], colorScheme: 'dark' },
  'desktop-light': { viewport: { width: 1280, height: 800 }, colorScheme: 'light' },
  'desktop-dark': { viewport: { width: 1280, height: 800 }, colorScheme: 'dark' },
};

// UX-2: the chat's states. Each one is a bridge with the scripted snapshot
// (committed events, live frames), the page query, and what to wait for.
const TRANSCRIPT_EVENTS = [
  { type: 'task', text: 'split the router into two crates' },
  { type: 'message', parent_id: null, message: { role: 'assistant', content:
    'The matcher is the largest piece, so it moves first.\n\nThe new entry point is:\n```rust\npub fn route(req: &Request) -> Option<Handler> {\n    table.get(&req.path).cloned()\n}\n```\nThen the tests run against both crates.' } },
  { type: 'task', text: 'go' },
  { type: 'message', parent_id: null, message: { role: 'assistant', content: 'Done. Both crates build and the router tests pass.' } },
];
const CHAT_STATES = [
  { name: '12-chat-empty', server: { quiet: true }, wait: 'a token-efficient Rust coding agent' },
  { name: '13-chat-transcript', server: { quiet: true, events: TRANSCRIPT_EVENTS }, wait: 'Both crates build' },
  { name: '14-chat-approval', server: { after: [{ type: 'approval_requested', request_id: 'req-1', call: { id: 'c1', name: 'edit', arguments: { path: 'src/router.rs' } } }] }, wait: 'allow once' },
  { name: '15-chat-plan', server: { quiet: true, after: [{ type: 'plan_proposed', items: [
    { text: 'Add the endpoint', done_criteria: 'curl returns 200', status: 'pending' },
    { text: 'Move the matcher', done_criteria: 'tests pass', status: 'pending' }] }] }, wait: 'revise' },
  { name: '16-chat-queued', server: { quiet: true, after: [{ type: 'queue_changed', queued: ['run the migration after the tests', 'draft the release note'] }] }, wait: '[2] draft' },
  { name: '17-chat-suggestions', server: { quiet: true }, wait: 'a token-efficient Rust coding agent', keys: '/co', wait2: '/compact - compact this session' },
  { name: '18-chat-error', server: { quiet: true, after: [{ type: 'error', message: 'the provider refused the request (429)' }] }, wait: 'refused the request' },
  { name: '19-chat-disconnected', server: { quiet: true }, url: 'ws://127.0.0.1:39998/', wait: 'disconnected' },
];

// Five sessions, two offline (the list's states).
const FIVE = [
  { id: 'aaaa', title: 'fix router', host_label: 'laptop', host_kind: 'local', viewer_key: 'vk1', state: 'idle',
    online: true, updated_at: '2026-11-15T10:20:30Z', open_url: '/app/?opened=fix' },
  { id: 'bbbb', title: 'old docs', host_label: 'cloud', host_kind: 'cloud', viewer_key: 'vk2', state: 'offline',
    online: false, updated_at: '2026-11-14T08:00:00Z', open_url: '/app/?opened=docs' },
  { id: 'cccc', title: 'release notes for the next minor version of the bridge', host_label: 'laptop', host_kind: 'local',
    viewer_key: 'vk3', state: 'running', online: true, updated_at: '2026-11-15T09:02:11Z', open_url: '/app/?opened=notes' },
  { id: 'dddd', title: 'triage', host_label: 'build box', host_kind: 'local', viewer_key: 'vk4', state: 'offline',
    online: false, updated_at: '2026-11-13T17:45:00Z', open_url: '/app/?opened=triage' },
  { id: 'eeee', title: '', host_label: 'laptop', host_kind: 'local', viewer_key: 'vk5', state: 'idle',
    online: true, updated_at: '2026-11-12T12:00:00Z', open_url: '/app/?opened=blank' },
];

const LOCAL_SESSIONS = [
  { id: '5b1e8f4a-2c3d-4e5f-8a9b-0c1d2e3f4a5b', title: 'fix the router crate', created_at: '', updated_at: '', message_count: 4 },
  { id: '9c2d7e10-1a2b-4c3d-8e4f-5a6b7c8d9e0f', title: 'write the release notes', created_at: '', updated_at: '', message_count: 2 },
];

const PROVIDERS = [
  { id: 'anthropic', label: 'Anthropic', status: 'configured', oauth_capable: true },
  { id: 'openai', label: 'OpenAI', status: 'not configured', oauth_capable: false },
];

// What the page reports for each shot: horizontal scroll and small targets.
async function audit(page) {
  return page.evaluate(() => {
    const root = document.documentElement;
    const small = [...document.querySelectorAll('[role=button]')]
      .map((e) => {
        const r = e.getBoundingClientRect();
        return { t: (e.innerText || '').trim().slice(0, 24), h: Math.round(r.height), w: Math.round(r.width), shown: r.width > 0 && r.height > 0 };
      })
      .filter((x) => x.shown && x.h < 44);
    return { overflowX: root.scrollWidth > root.clientWidth, small };
  });
}

async function main() {
  const outDir = process.argv[2];
  if (!outDir) {
    console.error('usage: node shots.mjs <outDir> [variant ...]');
    process.exit(2);
  }
  const wanted = process.argv.slice(3);
  const variants = wanted.length ? wanted : Object.keys(VARIANTS);
  for (const v of variants) if (!VARIANTS[v]) throw new Error('unknown variant ' + v);
  fs.mkdirSync(outDir, { recursive: true });

  const browser = await chromium.launch({ args: ['--no-sandbox'] });
  const failures = [];
  const report = [];
  const errorsSeen = [];
  try {
    for (const variant of variants) {
      const shoot = async (page, name) => {
        const file = path.join(outDir, `${name}-${variant}.png`);
        await page.waitForTimeout(400);
        await page.screenshot({ path: file });
        const a = await audit(page);
        report.push({ shot: `${name}-${variant}`, ...a });
      };
      const context = async (cookieUrl, site) => {
        const ctx = await browser.newContext(VARIANTS[variant]);
        if (cookieUrl) await ctx.addCookies([{ name: 'modron_owner', value: T.COOKIE, url: cookieUrl }]);
        const page = await ctx.newPage();
        page.on('pageerror', (e) => errorsSeen.push(`${variant}: ${e}`));
        page.on('console', (m) => {
          if (m.type() === 'error' && !/status of 40[0-9]|status of 503|429|WebSocket connection to 'ws:\/\/127\.0\.0\.1:39998/.test(m.text())) errorsSeen.push(`${variant}: console ${m.text()}`);
        });
        return { ctx, page };
      };
      const step = async (name, fn) => {
        try {
          await fn();
        } catch (e) {
          failures.push(`${variant} ${name}: ${e.message}\n${(e.stack || '').split('\n').slice(1, 4).join('\n')}`);
        }
      };

      // Portal: sign-in (empty and with an error), the session list (empty and five rows), Cloud.
      {
        const site = await T.startSite(await T.freePort(), T.newSiteState('on'));
        try {
          const { ctx, page } = await context(null, site);
          await step('sign-in', async () => {
            await page.goto(site.base + '/app/', { waitUntil: 'domcontentloaded' });
            await page.waitForSelector('input[type="password"]', { timeout: T.TIMEOUT });
            await shoot(page, '01-signin');
            await page.keyboard.type('wrong-token');
            await page.waitForFunction(() => document.activeElement && document.activeElement.value === 'wrong-token', null, { timeout: T.TIMEOUT });
            await page.keyboard.press('Enter');
            await page.waitForFunction(() => document.body.innerText.includes('wrong token'), null, { timeout: T.TIMEOUT });
            await shoot(page, '02-signin-error');
          });
          await ctx.close();
        } finally {
          site.close();
        }
      }
      for (const [name, rows] of [['03-sessions-empty', []], ['04-sessions-five', FIVE]]) {
        const site = await T.startSite(await T.freePort(), { ...T.newSiteState('on'), sessions: rows });
        try {
          const { ctx, page } = await context(site.base, site);
          await step(name, async () => {
            await page.goto(site.base + '/app/', { waitUntil: 'domcontentloaded' });
            await page.waitForFunction((s) => document.body.innerText.includes(s), rows.length ? 'fix router' : 'no sessions yet', { timeout: T.TIMEOUT });
            await shoot(page, name);
            if (name === '04-sessions-five') {
              await step('cloud', async () => {
                await T.tapLabel(page, 'cloud');
                await page.waitForFunction(() => document.body.innerText.includes('github accounts'), null, { timeout: T.TIMEOUT });
                await shoot(page, '05-cloud');
              });
            }
          });
          await ctx.close();
        } finally {
          site.close();
        }
      }

      // The owner's chat page (the live bridge): chat, settings, credentials, help, sessions.
      {
        const site = await T.startSite(await T.freePort(), T.newSiteState('off'));
        const ws = await T.startWsServer({ providers: PROVIDERS, sessions: LOCAL_SESSIONS });
        try {
          const { ctx, page } = await context(null, site);
          const query = `?url=${encodeURIComponent(ws.url)}&session=${T.SESSION}`;
          await step('chat', async () => {
            await page.goto(site.base + '/' + query, { waitUntil: 'domcontentloaded' });
            await page.waitForFunction((s) => document.body.innerText.includes(s), T.STREAM, { timeout: T.TIMEOUT });
            await shoot(page, '06-chat');
          });
          await step('settings', async () => {
            await T.tapLabel(page, 'settings');
            await page.waitForFunction(() => document.body.innerText.includes('approval mode'), null, { timeout: T.TIMEOUT });
            await shoot(page, '07-settings');
          });
          await step('credentials', async () => {
            // Esc backs out of Settings to the chat, where /auth opens the screen.
            await page.keyboard.press('Escape');
            await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: T.TIMEOUT });
            await page.keyboard.type('/auth');
            await page.waitForTimeout(300);
            await page.keyboard.press('Enter');
            await page.waitForFunction(() => document.body.innerText.includes('OpenAI: not configured'), null, { timeout: T.TIMEOUT });
            await shoot(page, '08-credentials');
          });
          await step('help', async () => {
            await T.tapLabel(page, 'help');
            await page.waitForTimeout(500);
            await shoot(page, '09-help');
          });
          await step('sessions', async () => {
            await T.tapLabel(page, 'sessions');
            await page.waitForFunction(() => document.body.innerText.includes('fix the router crate'), null, { timeout: T.TIMEOUT });
            await shoot(page, '10-sessions');
          });
          await ctx.close();
        } finally {
          ws.close();
          site.close();
        }
      }
      // UX-2: the chat's states, one bridge each.
      for (const state of CHAT_STATES) {
        const site = await T.startSite(await T.freePort(), T.newSiteState('off'));
        const ws = await T.startWsServer({ providers: PROVIDERS, sessions: LOCAL_SESSIONS, ...state.server });
        try {
          const { ctx, page } = await context(null, site);
          const url = state.url || ws.url;
          const query = `?url=${encodeURIComponent(url)}&session=${T.SESSION}`;
          await step(state.name, async () => {
            await page.goto(site.base + '/' + query, { waitUntil: 'domcontentloaded' });
            await page.waitForFunction((s) => document.body.innerText.includes(s), state.wait, { timeout: T.TIMEOUT });
            if (state.keys) {
              await page.waitForFunction(() => document.activeElement && ['INPUT', 'TEXTAREA'].includes(document.activeElement.tagName), null, { timeout: T.TIMEOUT });
              await page.keyboard.type(state.keys);
              await page.waitForFunction((s) => document.body.innerText.includes(s), state.wait2, { timeout: T.TIMEOUT });
            }
            await shoot(page, state.name);
          });
          await ctx.close();
        } finally {
          ws.close();
          site.close();
        }
      }
      // The owner's invite (Settings, a viewer link): the link must show whole.
      {
        const site = await T.startSite(await T.freePort(), T.newSiteState('on'));
        const ws = await T.startWsServer({ providers: PROVIDERS, sessions: LOCAL_SESSIONS });
        try {
          const { ctx, page } = await context(site.base, site);
          const query = `?url=${encodeURIComponent(ws.url)}&session=${T.SESSION}`;
          await step('invite', async () => {
            await page.goto(site.base + '/' + query, { waitUntil: 'domcontentloaded' });
            await page.waitForFunction((s) => document.body.innerText.includes(s), T.STREAM, { timeout: T.TIMEOUT });
            await T.tapLabel(page, 'settings');
            await T.tapLabel(page, 'viewer');
            await page.waitForFunction(() => /\/i\/0{31}1\.[0-9a-f]{64}/.test(document.body.innerText), null, { timeout: T.TIMEOUT });
            await shoot(page, '11-invite');
          });
          await ctx.close();
        } finally {
          ws.close();
          site.close();
        }
      }
    }
  } finally {
    await browser.close();
  }

  for (const r of report) {
    const small = r.small.map((x) => `${x.t || '(no label)'} ${x.w}x${x.h}`).join('; ');
    console.log(`${r.shot}: overflowX=${r.overflowX}${small ? ` small=[${small}]` : ''}`);
  }
  for (const e of errorsSeen) console.log('page error: ' + e);
  for (const f of failures) console.log('FAIL ' + f);
  console.log(`wrote ${report.length} screenshots to ${outDir}`);
  process.exit(failures.length ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
