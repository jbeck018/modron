// Measurement script for the NSH web spike. Usage: node measure.js <url>
// Requires Playwright (preinstalled, `require('playwright')`). Prints JSON.
const { chromium, devices } = require('playwright');

const url = process.argv[2];
if (!url) { console.error('usage: node measure.js <url>'); process.exit(1); }
const RUNS = 5;
const median = (a) => { const s = [...a].sort((x, y) => x - y); return s[Math.floor(s.length / 2)]; };
const pct = (a, p) => { const s = [...a].sort((x, y) => x - y); return s[Math.min(s.length - 1, Math.ceil(s.length * p) - 1)]; };
const mean = (a) => a.reduce((x, y) => x + y, 0) / a.length;
const round = (x) => Math.round(x * 100) / 100;

async function loadTime(browser, device) {
  const times = [];
  for (let i = 0; i < RUNS; i++) {
    const ctx = await browser.newContext(device || {});
    const page = await ctx.newPage();
    await page.goto(url, { waitUntil: 'commit' });
    await page.waitForFunction(() => document.body && document.body.innerText.includes('row 00000'), null, { timeout: 60000, polling: 'raf' });
    times.push(await page.evaluate(() => performance.now()));
    await ctx.close();
  }
  return { runs_ms: times.map(round), min_ms: round(Math.min(...times)), median_ms: round(median(times)) };
}

(async () => {
  const browser = await chromium.launch();
  const out = {};

  // Item 3: DOM inspection, desktop viewport.
  {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await ctx.newPage();
    await page.goto(url);
    await page.waitForFunction(() => document.body.innerText.includes('row 00000'), null, { timeout: 60000 });
    await page.waitForTimeout(500);
    out.dom = await page.evaluate(() => {
      const all = document.querySelectorAll('*');
      const tags = {};
      for (const el of all) tags[el.tagName.toLowerCase()] = (tags[el.tagName.toLowerCase()] || 0) + 1;
      const roles = {};
      for (const el of document.querySelectorAll('[role]')) roles[el.getAttribute('role')] = (roles[el.getAttribute('role')] || 0) + 1;
      const inputs = [...document.querySelectorAll('input,textarea,[contenteditable]')].map(e => ({ tag: e.tagName.toLowerCase(), type: e.type || null, role: e.getAttribute('role'), aria_label: e.getAttribute('aria-label') }));
      const sel = getComputedStyle(document.body).userSelect;
      return { element_count: all.length, canvas_count: document.querySelectorAll('canvas').length, tags, roles, inputs, body_user_select: sel, aria_live: document.querySelectorAll('[aria-live]').length };
    });
    await ctx.close();
  }

  // Item 4: load time.
  out.load_desktop = await loadTime(browser, { viewport: { width: 1280, height: 800 } });
  out.load_iphone13 = await loadTime(browser, devices['iPhone 13']);

  // Item 5: streaming frame times (desktop). Typed "stream" + Enter starts it.
  {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await ctx.newPage();
    await page.goto(url);
    await page.waitForFunction(() => document.body.innerText.includes('row 00000'), null, { timeout: 60000 });
    await page.waitForTimeout(500);
    const field = page.locator('input, textarea').first();
    await field.click();
    await page.keyboard.type('stream');
    await page.keyboard.press('Enter');
    const res = await page.evaluate(() => new Promise((resolve) => {
      const deltas = [];
      const t0 = performance.now();
      let last = null;
      const isDone = () => /tokens 2000 \|/.test(document.body.innerText);
      const step = (ts) => {
        if (last !== null) deltas.push(ts - last);
        last = ts;
        if (isDone() || performance.now() - t0 > 60000) {
          resolve({ deltas, total_ms: performance.now() - t0, done: isDone() });
        } else requestAnimationFrame(step);
      };
      requestAnimationFrame(step);
    }));
    const d = res.deltas.slice(1); // drop the first delta (frame gap before sampling)
    out.stream = {
      done_2000_tokens: res.done,
      total_ms: round(res.total_ms),
      frames: d.length,
      frame_mean_ms: round(mean(d)),
      frame_p95_ms: round(pct(d, 0.95)),
      frame_median_ms: round(median(d)),
    };
    await ctx.close();
  }

  // Item 6: input. Desktop typing + Enter, then mobile focus check.
  {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
    const page = await ctx.newPage();
    await page.goto(url);
    await page.waitForFunction(() => document.body.innerText.includes('row 00000'), null, { timeout: 60000 });
    await page.locator('input, textarea').first().click();
    await page.keyboard.type('hello spike');
    await page.keyboard.press('Enter');
    let received = true;
    try { await page.waitForFunction(() => document.body.innerText.includes('rows 2001'), null, { timeout: 5000 }); } catch (e) { received = false; }
    out.input_desktop = { received_row_status_2001: received, value_after_enter: await page.locator('input, textarea').first().inputValue().catch(() => 'n/a') };
    await ctx.close();
  }
  {
    const ctx = await browser.newContext(devices['iPhone 13']);
    const page = await ctx.newPage();
    await page.goto(url);
    await page.waitForFunction(() => document.body.innerText.includes('row 00000'), null, { timeout: 60000 });
    await page.locator('input, textarea').first().tap().catch(async () => page.locator('input, textarea').first().click());
    await page.waitForTimeout(300);
    out.input_iphone13 = await page.evaluate(() => {
      const a = document.activeElement;
      return { active_tag: a ? a.tagName.toLowerCase() : null, active_type: a ? a.type || null : null, is_text_entry: !!a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA' || a.isContentEditable), innerWidth: window.innerWidth, has_horizontal_scroll: document.documentElement.scrollWidth > document.documentElement.clientWidth };
    });
    await page.keyboard.type('hi phone');
    await page.keyboard.press('Enter');
    let received = true;
    try { await page.waitForFunction(() => document.body.innerText.includes('rows 2001'), null, { timeout: 5000 }); } catch (e) { received = false; }
    out.input_iphone13.received_row_status_2001 = received;
    await ctx.close();
  }

  await browser.close();
  console.log(JSON.stringify(out, null, 2));
})().catch((e) => { console.error('MEASURE ERROR', e); process.exit(1); });
