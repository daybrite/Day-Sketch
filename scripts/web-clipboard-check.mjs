// Real browser binary clipboard → canvas → editable copy/paste → SQLite/OPFS reload.
// After `day build -p web-dom`:
// DAY_WEB_DRIVER_PLAYWRIGHT=<dir containing node_modules/playwright> node scripts/web-clipboard-check.mjs [dist] [chromium|webkit]
import { createRequire } from 'node:module';
import http from 'node:http';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import assert from 'node:assert/strict';

const require = createRequire((process.env.DAY_WEB_DRIVER_PLAYWRIGHT ?? process.cwd()) + '/');
const playwright = require('playwright');
const dist = path.resolve(process.argv[2] ?? 'build/day/cargo/web-dom/debug/dist');
const engine = process.argv[3] ?? 'chromium';
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm', '.css': 'text/css', '.json': 'application/json' };
const server = http.createServer((req, res) => {
  const file = path.join(dist, req.url.split('?')[0] === '/' ? 'index.html' : req.url.split('?')[0]);
  try {
    const body = fs.readFileSync(file);
    res.writeHead(200, { 'Content-Type': mime[path.extname(file)] ?? 'application/octet-stream',
      'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp' });
    res.end(body);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'day-sketch-clipboard-'));
let context;
try {
  context = await playwright[engine].launchPersistentContext(profile, { headless: true, ...(process.env.DAY_WEB_BROWSER_EXECUTABLE ? { executablePath: process.env.DAY_WEB_BROWSER_EXECUTABLE } : {}), viewport: { width: 1100, height: 800 } });
  context.setDefaultTimeout(30000);
  const page = context.pages()[0] ?? await context.newPage();
  const errors = [];
  page.on('pageerror', error => { errors.push(error.message); console.error(error.message); });
  page.on('console', message => { if (message.type() === 'error') console.error(message.text()); });
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.waitForFunction(() => document.querySelector('#sk-doc')?.textContent.includes('sketch-default'));
  console.log('OPFS document opened');
  await context.grantPermissions(['clipboard-read','clipboard-write']);
  await page.evaluate(async base64 => {
    const bytes = Uint8Array.from(atob(base64), c => c.charCodeAt(0));
    await navigator.clipboard.write([new ClipboardItem({'image/png':new Blob([bytes],{type:'image/png'})})]);
  }, fs.readFileSync('resource/images/app_logo.png').toString('base64'));
  await page.locator('#canvas').click({position:{x:20,y:20}});
  await page.keyboard.press('Meta+v');
  await page.locator('#insp-image-op').waitFor();
  console.log('Image inspector appeared');
  const edit = async (id, value) => { await page.locator(`#${id}`).fill(value); await page.locator(`#${id}`).press('Enter'); };
  await edit('insp-x', '100'); await edit('insp-y', '80');
  await edit('insp-w', '200'); await edit('insp-h', '160');
  await edit('insp-rotation', '30'); await edit('insp-image-op', '50%');
  await page.waitForFunction(() => document.querySelector('#sk-frame')?.textContent === '100,80 200x160');
  // The center of the image contains color; wait for createImageBitmap and replay, not just
  // for the model row or inspector, which can exist before the browser finishes decoding.
  const hasImagePixels = async () => page.waitForFunction(() => {
    const canvas = document.querySelector('#canvas');
    const data = canvas.getContext('2d').getImageData(Math.round(200 * devicePixelRatio), Math.round(160 * devicePixelRatio), 1, 1).data;
    return data[0] < 245 || data[1] < 245 || data[2] < 245;
  });
  await hasImagePixels();
  fs.mkdirSync('build/day/screenshots/web-clipboard', { recursive: true });
  await page.screenshot({ path: 'build/day/screenshots/web-clipboard/inserted.png' });
  const rect = await page.locator('#canvas').boundingBox();
  await page.mouse.click(rect.x+200,rect.y+160);
  await page.keyboard.press('Meta+c');
  // Copy publishes native event data and then the encoded PNG asynchronously. Read
  // the bytes in the polling condition: a ClipboardItem can become stale between
  // listing its types and getType() while the async publication replaces it.
  await page.waitForFunction(async () => {
    try {
      for (const item of await navigator.clipboard.read()) {
        if (item.types.includes('text/html') && item.types.includes('image/png')
            && (await item.getType('image/png')).size > 0) return true;
      }
    } catch (error) {
      if (error.name !== 'InvalidStateError') throw error;
    }
    return false;
  });
  await page.keyboard.press('Meta+v');
  await page.waitForFunction(() => document.querySelector('#sk-count')?.textContent.includes('2'));
  assert.equal(await page.locator('#sk-frame').textContent(), '116,96 200x160');
  await page.keyboard.press('Meta+z');
  await page.waitForFunction(() => document.querySelector('#sk-count')?.textContent.includes('1'));
  // A different JS heap and bitmap registry: only the BLOB in OPFS can restore the pixels.
  console.log('Image drawn; reloading');
  await page.reload();
  await page.waitForFunction(() => document.querySelector('#sk-count')?.textContent.includes('1'));
  await hasImagePixels();
  const box = await page.locator('#canvas').boundingBox();
  await page.mouse.click(box.x + 200, box.y + 160);
  await page.locator('#insp-image-op').waitFor();
  console.log('Image inspector appeared');
  // Fluent wraps interpolated numbers with Unicode direction isolates.
  const plain = text => text.replace(/[\u2066-\u2069]/g, '');
  assert.equal(plain(await page.locator('#insp-image-op').inputValue()), '50%');
  assert.equal(await page.locator('#insp-rotation').inputValue(), '30');
  assert.equal(await page.locator('#sk-frame').textContent(), '100,80 200x160');
  await page.screenshot({ path: 'build/day/screenshots/web-clipboard/reopened.png' });
  assert.deepEqual(errors, []);
  console.log(`${engine}: external PNG paste, native PNG copy, editable copy/paste, undo and OPFS reload passed`);
} finally {
  if (context) await context.close();
  await new Promise(resolve => server.close(resolve));
  fs.rmSync(profile, { recursive: true, force: true });
}
