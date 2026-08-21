// The web persistence proof (docs/web.md): REAL browser input — mouse clicks and drags, not
// injected dayscript events — places and moves a shape, then a real page.reload() must find
// the drawing again out of the origin's OPFS, through the day-sql worker channel.
//
// Run after `day build -p web-dom` (the dist this serves):
//   DAY_WEB_DRIVER_PLAYWRIGHT=<dir with node_modules/playwright> \
//     node scripts/web-reload-check.mjs build/day/cargo/web-dom/debug/dist [webkit|chromium]
//
// Serves the dist with the same COOP/COEP headers the day server sends (the channel needs
// cross-origin isolation) and uses a THROWAWAY persistent profile — ephemeral WebKit
// contexts have no storage backing at all, so OPFS would be absent rather than exercised.

import { createRequire } from 'node:module';
import http from 'node:http';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const require = createRequire((process.env.DAY_WEB_DRIVER_PLAYWRIGHT ?? process.cwd()) + '/');
const playwright = require('playwright');

const dist = process.argv[2] ?? 'build/day/cargo/web-dom/debug/dist';
const engine = process.argv[3] ?? 'webkit';

const mime = {
  '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm',
  '.css': 'text/css', '.json': 'application/json', '.svg': 'image/svg+xml',
};
const srv = http.createServer((req, res) => {
  let p = req.url.split('?')[0];
  if (p === '/') p = '/index.html';
  try {
    const body = fs.readFileSync(path.join(dist, p));
    res.writeHead(200, {
      'Content-Type': mime[path.extname(p)] ?? 'application/octet-stream',
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
      'Cache-Control': 'no-store',
    });
    res.end(body);
  } catch {
    res.writeHead(404);
    res.end();
  }
});
await new Promise((r) => srv.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${srv.address().port}/`;

const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'day-sketch-reload-'));
const ctx = await playwright[engine].launchPersistentContext(profile, {
  headless: true,
  viewport: { width: 1000, height: 720 },
});
const page = ctx.pages()[0] ?? (await ctx.newPage());
// Console capture — the day-sql worker logs traced statements here in debug builds.
const consoleLines = [];
page.on('console', (m) => consoleLines.push(m.text()));

let failures = 0;
const check = (name, ok, got) => {
  console.log(`${ok ? '✓' : '✗'} ${name}${ok ? '' : ` — got ${JSON.stringify(got)}`}`);
  if (!ok) failures += 1;
};
const textOf = (id) => page.evaluate((i) => document.getElementById(i)?.textContent ?? '', id);
const waitText = async (id, want, ms = 30000) => {
  const deadline = Date.now() + ms;
  let got = '';
  while (Date.now() < deadline) {
    got = await textOf(id);
    if (got.includes(want)) return got;
    await page.waitForTimeout(200);
  }
  return got;
};
// Canvas-local coordinates → a real mouse event at the right page position.
const canvasBox = () => page.locator('#canvas').boundingBox();

await page.goto(url);

// The persistent document opened synchronously through the worker — not the memory fallback.
check('opens the OPFS document', (await waitText('sk-doc', 'sketch-default')).includes('sketch-default'));

// Arm the rectangle tool and place a shape with a REAL click.
await page.locator('#tool-rect').click();
const box = await canvasBox();
await page.mouse.click(box.x + 60, box.y + 60);
check('real click places a shape', (await waitText('sk-count', '1')).includes('1'));
check('placed at the click point', (await waitText('sk-frame', '60,60 96x64')).includes('60,60 96x64'));

// A real drag moves it — live previews, one undo unit.
await page.mouse.move(box.x + 108, box.y + 92);
await page.mouse.down();
await page.mouse.move(box.x + 158, box.y + 142, { steps: 12 });
await page.mouse.up();
check('real drag moves it', (await waitText('sk-frame', '110,110 96x64')).includes('110,110 96x64'));

// Real clicks on Undo / Redo.
await page.locator('#sk-undo').click();
check('undo restores the frame', (await waitText('sk-frame', '60,60 96x64')).includes('60,60 96x64'));
await page.locator('#sk-redo').click();
check('redo reapplies the move', (await waitText('sk-frame', '110,110 96x64')).includes('110,110 96x64'));

// THE proof: a real reload finds the same drawing in OPFS.
await page.reload();
check('reload reopens the document', (await waitText('sk-doc', 'sketch-default')).includes('sketch-default'));
check('the shape survived', (await waitText('sk-count', '1')).includes('1'));
const box2 = await canvasBox();
await page.mouse.click(box2.x + 150, box2.y + 140);
check('at its moved position', (await waitText('sk-frame', '110,110 96x64')).includes('110,110 96x64'));

// And the scene is still editable after reload — the reopened container accepts writes.
await page.locator('#tool-oval').click();
await page.mouse.click(box2.x + 300, box2.y + 200);
check('still editable after reload', (await waitText('sk-count', '2')).includes('2'));

// Debug builds trace every statement through the engine (docs/persistence.md): the day-sql
// worker's `[day-sql]` lines land in the console, parameters expanded.
{
  const deadline = Date.now() + 10000;
  let hit = false;
  while (Date.now() < deadline && !hit) {
    hit = consoleLines.some((l) => l.includes('[day-sql]') && l.includes('INSERT INTO nodes'));
    if (!hit) await page.waitForTimeout(200);
  }
  check('sql statements log to the console', hit, consoleLines.filter((l) => l.includes('[day-sql]')).slice(0, 3));
}

// Oval resize handles with REAL input. The corner handles sit on the bounding box, outside
// the ellipse — the regression this guards: the shim once fired the tap on pointerdown, so
// pressing a handle outside the shape's geometry cleared the selection before the drag began.
// A real click outside deselects; a real click inside the ellipse selects; then a real drag
// on the bottom-right handle (at the bbox corner, outside the ellipse) must resize.
await page.mouse.click(box2.x + 550, box2.y + 60);
check('empty click deselects', (await waitText('sk-sel', 'Nothing')).includes('Nothing'));
await page.mouse.click(box2.x + 348, box2.y + 232);
check('click inside the oval selects it', (await waitText('sk-frame', '300,200 96x64')).includes('300,200 96x64'));
await page.mouse.move(box2.x + 396, box2.y + 264);
await page.mouse.down();
await page.mouse.move(box2.x + 446, box2.y + 314, { steps: 10 });
await page.mouse.up();
check('handle outside the ellipse resizes', (await waitText('sk-frame', '300,200 146x114')).includes('300,200 146x114'));

// The browser's own edit-command route: REAL copy/cut/paste ClipboardEvents through the
// document listeners (the same path ⌘C/⌘V takes). The oval from the resize check is still
// selected; copying it must yield SVG, and pasting that SVG must land a new shape.
const copied = await page.evaluate(() => {
  const dt = new DataTransfer();
  document.dispatchEvent(new ClipboardEvent('copy', { clipboardData: dt, bubbles: true, cancelable: true }));
  return dt.getData('text/plain');
});
if (engine === 'chromium') {
  // WebKit's constructed ClipboardEvent carries no clipboardData; there the page-local
  // mirror transports it (asserted by the paste below), so the format check is Chromium's.
  check('copy event yields SVG', copied.includes('<svg') && copied.includes('<ellipse'), copied.slice(0, 120));
}
await page.evaluate((s) => {
  const dt = new DataTransfer();
  dt.setData('text/plain', s);
  document.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
}, copied);
check('paste event lands the copy', (await waitText('sk-count', '3')).includes('3'));

// The standard undo keys as REAL keydowns — the browser has no document undo of its own, so
// ⌘Z/⇧⌘Z are bound by the shim (outside editable elements).
await page.keyboard.press('Meta+z');
check('⌘Z undoes the paste', (await waitText('sk-count', '2')).includes('2'));
await page.keyboard.press('Meta+Shift+z');
check('⇧⌘Z redoes it', (await waitText('sk-count', '3')).includes('3'));

// Modifier multi-select with REAL input: click the rect, shift-click the first oval.
await page.mouse.click(box2.x + 150, box2.y + 140);
check('plain click selects the rect', (await waitText('sk-frame', '110,110 96x64')).includes('110,110'));
await page.keyboard.down('Shift');
await page.mouse.click(box2.x + 348, box2.y + 232);
await page.keyboard.up('Shift');
// The pasted oval sits topmost over that point, so it is the one shift-click adds.
check('shift-click adds to the selection', (await waitText('sk-sel', '1,3')).includes('1,3'));

// The platform's own Select All shortcut.
await page.keyboard.press('Meta+a');
check('⌘A selects everything', (await waitText('sk-sel', 'Selected:')).split(',').length >= 3);

// Arrow-key nudging on a single shape: 1px, then 10 with shift, then back.
await page.mouse.click(box2.x + 150, box2.y + 140);
check('single selection again', (await waitText('sk-frame', '110,110 96x64')).includes('110,110'));
await page.keyboard.press('ArrowRight');
check('arrow nudges 1px', (await waitText('sk-frame', '111,110 96x64')).includes('111,110'));
await page.keyboard.press('Shift+ArrowDown');
check('shifted arrow nudges 10px', (await waitText('sk-frame', '111,120 96x64')).includes('111,120'));
await page.keyboard.press('ArrowLeft');
await page.keyboard.press('Shift+ArrowUp');
check('nudges reverse cleanly', (await waitText('sk-frame', '110,110 96x64')).includes('110,110'));

await ctx.close();
srv.close();
fs.rmSync(profile, { recursive: true, force: true });
console.log(failures === 0 ? 'PASS' : `FAIL (${failures})`);
process.exit(failures === 0 ? 0 : 1);
