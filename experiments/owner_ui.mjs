// Optional DOM smoke test: one isolated headless Chrome, no Electron, no audio.
// Built-in Node modules only. Backend behavior is tested separately in Rust.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, readFile, rm, mkdir, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const [chromePath, outputPath] = process.argv.slice(2);
if (!chromePath || !outputPath) throw new Error('Usage: node experiments/owner_ui.mjs CHROME_BINARY OUTPUT_DIRECTORY');
const source = fileURLToPath(new URL('../src/owner/', import.meta.url));
const profile = await mkdtemp(join(tmpdir(), 'sidecar-ui-'));
const output = resolve(outputPath);
await mkdir(output, { recursive: true });
let signedIn = false;
let ownerConfigured = false;
let saved = { revision: 'initial', settings: { navidrome_url: null, listeners: [] } };
let errorNext = null;
let held = null;
let delayNext = false;
let saves = 0;
let discovery = { revision: 'initial', status: 'not-published', hasAnnouncement: false };
let publicationRequests = 0;
let storage = { settings: { revision: 'initial', enabled: true, limit_bytes: 5e9, clear_epoch: 'initial' },
  cache: { retained_bytes: 123e6, entries: 32, problem: false }, working_bytes: 64e6 };
storage.resources = {revision:'initial',max_conversions:null,working_bytes:512e6};
storage.runtime = {detected_cpus:4,conversions:{limit:2,running:0,waiting:0},working:{reserved_bytes:96e6,limit_bytes:512e6,free_reserve_bytes:512*1024*1024}};
let resourceRequests = 0;
let storageRequests = 0;
let dashboardRequests = 0;
let dashboardError = null;
let holdDashboard = false;
let releaseDashboard = null;
let activity = { clients: [] };
let rate = 0;
let health = {status:'responding',checked_seconds_ago:2};
const stage = () => !saved.settings.navidrome_url ? 'navidrome' : saved.settings.listeners.length ? 'connections-saved' : 'listeners';
const server = createServer(async (request, response) => {
  response.setHeader('Content-Security-Policy', "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'");
  const send = (status, value) => { response.writeHead(status, { 'Content-Type': 'application/json' }); response.end(JSON.stringify(value)); };
  try {
    if (!request.url.startsWith('/admin/api/')) {
      const path = { '/': 'index.html', '/owner.js': 'owner.js', '/owner.css': 'owner.css' }[request.url];
      if (!path) { response.writeHead(404); response.end(); return; }
      response.writeHead(200, { 'Content-Type': path.endsWith('.js') ? 'text/javascript' : path.endsWith('.css') ? 'text/css' : 'text/html' });
      response.end(await readFile(join(source, path))); return;
    }
    let text = '';
    for await (const chunk of request) { text += chunk; if (text.length > 32768) throw new Error('large request'); }
    const body = text ? JSON.parse(text) : {};
    const path = request.url.slice('/admin/api/'.length);
    if (path === 'status') return send(200, { ownerConfigured });
    if (path === 'claim') { ownerConfigured = true; signedIn = true; return send(200, {}); }
    if (path === 'login') { signedIn = true; return send(200, {}); }
    if (path === 'logout') { signedIn = false; return send(200, {}); }
    if (!signedIn) return send(401, { error: 'Sign in again.' });
    if (path === 'dashboard') {
      dashboardRequests++;
      const value = structuredClone({ready:true,activity,bytes_per_second:rate,rate_window_seconds:5,storage,navidrome:health});
      const error = dashboardError; dashboardError = null;
      if (holdDashboard) {
        holdDashboard = false;
        await new Promise((resolve) => { releaseDashboard = resolve; });
      }
      return error ? send(error.status,{error:error.message}) : send(200,value);
    }
    if (path === 'setup') return send(200, { stage: stage(), setup: saved, discovery, storage });
    if (path === 'discovery') publicationRequests++;
    if (errorNext) { const error = errorNext; errorNext = null; return send(error.status, { error: error.message }); }
    if (delayNext) { delayNext = false; await new Promise((resolve) => { held = resolve; }); }
    if (path === 'resources') {
      if (body.revision !== storage.resources.revision) return send(409,{error:'Reload saved settings.'});
      resourceRequests++;
      storage.resources = {...body,revision:String(resourceRequests)};
      storage.runtime.conversions.limit = body.max_conversions ?? 2;
      storage.runtime.working.limit_bytes = body.working_bytes;
      return send(200,storage);
    }
    if (path === 'storage') {
      if (request.method === 'GET') return send(200, storage);
      storageRequests++;
      if (body.revision !== storage.settings.revision) return send(409, { error: 'Reload saved settings.' });
      if ((body.clear || (!body.enabled && storage.settings.enabled)) && !body.confirmed) return send(400, { error: 'Confirm clearing retained audio.' });
      if (body.clear || !body.enabled) { storage.cache.retained_bytes = 0; storage.cache.entries = 0; }
      storage.settings = { ...storage.settings, ...body, revision: String(storageRequests) };
      return send(200, storage);
    }
    if (path === 'discovery') {
      if (body.setup_revision !== saved.revision || body.discovery_revision !== discovery.revision) return send(409, { error: 'Reload saved settings.' });
      if (body.listeners) saved = { revision: String(++saves), settings: { ...saved.settings, listeners: body.listeners } };
      discovery = { revision: String(publicationRequests), status: body.action === 'publish' ? 'published' : 'manual',
        hasAnnouncement: body.action === 'publish', target: saved.settings.navidrome_url };
      return send(200, { stage: stage(), setup: saved, discovery });
    }
    if (path === 'setup/check') return send(200, { authenticated: false });
    if (body.revision !== saved.revision) return send(409, { error: 'Setup changed elsewhere. Reload the saved settings before trying again.' });
    const settings = structuredClone(saved.settings);
    if (path === 'setup/navidrome') settings.navidrome_url = body.url.replace(/\/+$/, '');
    else if (path === 'setup/listeners') settings.listeners = body.listeners.map((item) => ({ ...item, url: item.url.replace(/\/+$/, '') }));
    else return send(404, {});
    saves++;
    saved = { revision: String(saves), settings };
    send(200, { stage: stage(), setup: saved, detection: { authenticated: false } });
  } catch { send(500, { error: 'Test fixture failed.' }); }
});
let browser;
let watchdog;
let seq = 0;
const pending = new Map();
let session;
let wire = Buffer.alloc(0);
const errors = [];
function stop() {
  if (browser?.pid) { try { process.kill(-browser.pid, 'SIGKILL'); } catch {} }
  server.closeAllConnections(); server.close();
}
try {
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  browser = spawn(chromePath, ['--headless=new', '--no-first-run', '--no-default-browser-check', '--disable-background-networking', '--disable-component-update', '--disable-sync', '--disable-extensions', '--remote-debugging-pipe', `--user-data-dir=${profile}`, 'about:blank'], {
    detached: true, stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe'],
  });
  browser.on('error', (error) => { errors.push(error.message); });
  watchdog = setTimeout(() => { stop(); process.exitCode = 1; }, 45000);
  browser.stdio[4].on('data', (chunk) => {
    wire = Buffer.concat([wire, chunk]);
    for (let end; (end = wire.indexOf(0)) >= 0;) {
      const message = JSON.parse(wire.subarray(0, end).toString()); wire = wire.subarray(end + 1);
      if (message.id) {
        const item = pending.get(message.id); pending.delete(message.id);
        if (item) { clearTimeout(item.timeout); message.error ? item.reject(new Error(JSON.stringify(message.error))) : item.resolve(message.result); }
      }
      if (message.method === 'Runtime.exceptionThrown') errors.push(JSON.stringify(message.params.exceptionDetails));
    }
  });
  function cdp(method, params = {}, withSession = true) {
    const id = ++seq;
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 5000);
      pending.set(id, { resolve, reject, timeout });
      browser.stdio[3].write(JSON.stringify({ id, method, params, ...(withSession && session ? { sessionId: session } : {}) }) + '\0');
    });
  }
  const target = await cdp('Target.createTarget', { url: 'about:blank' }, false);
  session = (await cdp('Target.attachToTarget', { targetId: target.targetId, flatten: true }, false)).sessionId;
  await cdp('Page.enable'); await cdp('Runtime.enable');
  await cdp('Page.addScriptToEvaluateOnNewDocument', {source: 'window.policyViolations = []; document.addEventListener("securitypolicyviolation", event => policyViolations.push(event.violatedDirective));'});
  await cdp('Emulation.setEmulatedMedia',{features:[{name:'prefers-reduced-motion',value:'reduce'}]});
  await cdp('Emulation.setDeviceMetricsOverride', { width: 1100, height: 1000, deviceScaleFactor: 1, mobile: false });
  async function evaluate(expression) {
    const result = await cdp('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  }
  const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  async function until(expression) {
    const end = Date.now() + 5000;
    while (!(await evaluate(expression))) { if (Date.now() > end) throw new Error(`Timed out: ${expression}`); await pause(20); }
  }
  async function click(id) {
    assert.equal(await evaluate(`document.getElementById(${JSON.stringify(id)}).checkVisibility() && !document.getElementById(${JSON.stringify(id)}).disabled`), true, `Visible enabled control: ${id}`);
    await evaluate(`document.getElementById(${JSON.stringify(id)}).click()`);
  }
  async function fill(selector, value) { await evaluate(`document.querySelector(${JSON.stringify(selector)}).value = ${JSON.stringify(value)}`); }
  async function submit(id) {
    assert.equal(await evaluate(`document.getElementById(${JSON.stringify(id)}).checkVisibility()`), true, `Visible form: ${id}`);
    await evaluate(`document.getElementById(${JSON.stringify(id)}).requestSubmit()`);
  }
  async function idle() { await until('!busy'); }
  async function shot(name) { const value = await cdp('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true }); await writeFile(join(output, `${name}.png`), Buffer.from(value.data, 'base64')); }
  await cdp('Page.navigate', { url: `http://127.0.0.1:${server.address().port}/` });
  await until('typeof busy !== "undefined" && !busy && document.getElementById("owner-form").checkVisibility()');
  await shot('owner-setup');
  await fill('#code', 'example-code'); await fill('#password','disposable fake password'); await fill('#confirm','disposable fake password');
  await submit('owner-form'); await idle();
  assert.equal(await evaluate('document.getElementById("setup-progress").checkVisibility()'),true);
  assert.equal(await evaluate('document.querySelector("[aria-current=step]").dataset.step'),'navidrome');
  await shot('navidrome-setup');
  console.log('PASS owner claim leads into guided setup with active progress');
  await fill('#navidrome-url', 'http://navidrome:4533/music/');
  errorNext = { status: 502, message: 'Couldn’t connect to Navidrome.' };
  await submit('navidrome-form'); await idle();
  assert.equal(saved.revision, 'initial');
  assert.equal(await evaluate('document.getElementById("navidrome-url").value'), 'http://navidrome:4533/music/');
  console.log('PASS failed connection keeps input and saved settings');
  delayNext = true;
  await submit('navidrome-form');
  await until('busy');
  assert.equal(await evaluate('document.getElementById("navidrome-url").disabled && document.getElementById("logout").disabled'), true);
  const heldDeadline = Date.now() + 5000;
  while (!held) {
    if (Date.now() > heldDeadline) throw new Error('Fixture did not receive the pending check');
    await pause(10);
  }
  held(); held = null; await idle();
  assert.equal(await evaluate('document.getElementById("listeners-section").hidden'), false);
  assert.equal(await evaluate('document.querySelector("[aria-current=step]").dataset.step'),'listeners');
  assert.equal(await evaluate('document.getElementById("message").textContent'),'');
  console.log('PASS pending check locks its form and advances without a redundant success paragraph');
  await cdp('Page.reload'); await until('typeof busy !== "undefined" && !busy && !document.getElementById("listeners-section").hidden');
  console.log('PASS reload resumes at listener addresses');
  assert.equal(await evaluate('document.querySelector(".remove-address").checkVisibility()'),false);
  assert.equal(await evaluate('document.querySelector(".address-row details") === null'),true);
  await shot('single-listener-setup');
  await fill('#addresses input', 'https://home.lan:7843/');
  await click('add-address');
  await fill('#addresses fieldset:last-child input', 'https://public.example/stratum/');
  await evaluate('document.querySelector("#addresses fieldset:last-child [data-mode=https_proxy]").click()');
  assert.equal(await evaluate('document.querySelector("#addresses fieldset:last-child select").value'),'https_proxy');
  assert.equal(await evaluate('document.querySelector("#addresses fieldset:last-child label .hint").checkVisibility()'),true);
  await evaluate('document.querySelector("#addresses fieldset:first-child [data-mode=sidecar_key]").focus()');
  await cdp('Input.dispatchKeyEvent',{type:'keyDown',key:'Tab',code:'Tab',windowsVirtualKeyCode:9});
  await cdp('Input.dispatchKeyEvent',{type:'keyUp',key:'Tab',code:'Tab',windowsVirtualKeyCode:9});
  assert.equal(await evaluate('document.activeElement.dataset.mode'),'https_proxy');
  await cdp('Input.dispatchKeyEvent',{type:'keyDown',key:'Enter',code:'Enter',windowsVirtualKeyCode:13,text:'\r',unmodifiedText:'\r'});
  await cdp('Input.dispatchKeyEvent',{type:'keyUp',key:'Enter',code:'Enter',windowsVirtualKeyCode:13});
  assert.equal(await evaluate('document.querySelector("#addresses select").value'),'https_proxy');
  await evaluate('document.querySelector("#addresses [data-mode=sidecar_key]").click()');
  console.log('PASS connection choices work by keyboard and show only relevant help');
  await shot('listener-setup');
  await submit('listeners-form'); await idle();
  assert.deepEqual(saved.settings.listeners, [{url:'https://home.lan:7843',trust:'sidecar_key'}, {url:'https://public.example/stratum',trust:'https_proxy'}]);
  assert.equal(await evaluate('document.getElementById("discovery-section").checkVisibility()'), true);
  await shot('discovery-setup');
  await click('publish-discovery');
  await fill('#navidrome-admin','admin'); await fill('#navidrome-password','fake password');
  errorNext = {status:502,message:'Could not publish. Try again.'};
  discovery.status = 'problem'; discovery.error = 'Could not publish. Try again.';
  await submit('discovery-form'); await idle();
  assert.equal(await evaluate('document.getElementById("discovery-section").checkVisibility()'),true);
  assert.equal(await evaluate('document.getElementById("navidrome-password").value'),'');
  assert.equal(await evaluate('document.getElementById("manual-discovery").checkVisibility()'),true);
  await cdp('Page.reload'); await until('typeof busy !== "undefined" && !busy && document.getElementById("discovery-section").checkVisibility()');
  console.log('PASS a failed discovery attempt stays in setup across reload with retry and manual alternatives');
  console.log('PASS primary and additional trust modes save and render');
  await click('manual-discovery'); await idle();
  assert.equal(discovery.status, 'manual');
  assert.equal(await evaluate('document.getElementById("discovery-form").hidden'), true);
  console.log('PASS manual selection finishes setup without administrator credentials');
  assert.equal(await evaluate('document.getElementById("overview-panel").checkVisibility()'),true);
  assert.equal(await evaluate('document.getElementById("settings-panel").checkVisibility()'),false);
  await click('nav-settings');
  assert.equal(await evaluate('document.getElementById("setup-progress").hidden'),true);
  await click('publish-discovery');
  await fill('#navidrome-admin', 'admin'); await fill('#navidrome-password', 'fake-nav-password');
  errorNext = { status: 400, message: 'Navidrome refused this sign-in.' };
  await submit('discovery-form'); await idle();
  assert.equal(discovery.status, 'manual');
  assert.equal(await evaluate('document.getElementById("navidrome-password").value'), '');
  assert.match(await evaluate('document.getElementById("message").textContent'), /refused/);
  console.log('PASS failed publication clears credentials and keeps manual configuration');
  await click('publish-discovery');
  await fill('#navidrome-admin', 'admin'); await fill('#navidrome-password', 'fake-nav-password');
  delayNext = true;
  await submit('discovery-form'); await until('busy');
  assert.equal(await evaluate('document.getElementById("navidrome-password").value'), '');
  const publicationDeadline = Date.now() + 5000;
  while (!held) { if (Date.now() > publicationDeadline) throw new Error('Publication did not reach fixture'); await pause(10); }
  held(); held = null; await idle();
  assert.equal(discovery.status, 'published');
  await shot('discovery-published');
  const beforeAddresses = structuredClone(saved.settings.listeners);
  const beforeAddressPublishes = publicationRequests;
  await click('edit-listeners');
  await submit('listeners-form'); await idle();
  assert.equal(publicationRequests, beforeAddressPublishes, 'unchanged addresses need no sign-in');
  await click('edit-listeners');
  await fill('#addresses input', 'https://corrected.lan:7843');
  await submit('listeners-form'); await idle();
  assert.equal(await evaluate('document.getElementById("discovery-form").checkVisibility()'), true);
  assert.equal(await evaluate('document.getElementById("listeners-form").hidden'), true);
  assert.equal(await evaluate('document.getElementById("discovery-submit").textContent'), 'Update and save');
  assert.match(await evaluate('document.getElementById("discovery-effect").textContent'), /new addresses/);
  assert.equal(publicationRequests, beforeAddressPublishes);
  assert.deepEqual(saved.settings.listeners, beforeAddresses);
  assert.equal(await evaluate('document.activeElement.id'), 'navidrome-admin');
  await shot('addresses-sign-in');
  await cdp('Emulation.setEmulatedMedia', {features:[{name:'prefers-color-scheme',value:'light'}]});
  await shot('addresses-sign-in-light');
  await cdp('Emulation.setEmulatedMedia', {features:[{name:'prefers-color-scheme',value:'dark'}]});
  await cdp('Emulation.setDeviceMetricsOverride', { width: 400, height: 900, deviceScaleFactor: 1, mobile: false });
  await shot('addresses-sign-in-dark-narrow');
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true);
  await cdp('Emulation.setEmulatedMedia', {features:[{name:'prefers-color-scheme',value:'light'}]});
  await cdp('Emulation.setDeviceMetricsOverride', { width: 1100, height: 1000, deviceScaleFactor: 1, mobile: false });
  await fill('#navidrome-admin', 'admin'); await fill('#navidrome-password', 'discard on cancel');
  await click('discovery-cancel');
  assert.equal(await evaluate('document.getElementById("navidrome-password").value'), '');
  assert.equal(await evaluate('document.querySelector("#addresses input").value'), 'https://corrected.lan:7843');
  assert.equal(await evaluate('document.activeElement.id'), 'listeners-save');
  assert.deepEqual(saved.settings.listeners, beforeAddresses);
  await submit('listeners-form'); await idle();
  errorNext = {status:400,message:'Navidrome refused this sign-in.'};
  await fill('#navidrome-admin', 'admin'); await fill('#navidrome-password', 'fake-nav-password');
  await submit('discovery-form'); await idle();
  assert.equal(await evaluate('document.getElementById("discovery-form").checkVisibility()'), true);
  assert.equal(await evaluate('document.getElementById("navidrome-password").value'), '');
  assert.deepEqual(saved.settings.listeners, beforeAddresses);
  assert.match(await evaluate('document.getElementById("message").textContent'), /refused/);
  await shot('addresses-failed-sign-in');
  delayNext = true;
  await fill('#navidrome-admin', 'admin'); await fill('#navidrome-password', 'fake-nav-password');
  await submit('discovery-form'); await until('busy');
  for (let tries=0; !held && tries<100; tries++) await pause(10);
  assert.ok(held);
  assert.deepEqual(saved.settings.listeners, beforeAddresses);
  assert.equal(await evaluate('document.getElementById("navidrome-password").value'), '');
  held(); held = null; await idle();
  assert.equal(saved.settings.listeners[0].url, 'https://corrected.lan:7843');
  assert.equal(discovery.status, 'published');
  assert.equal(await evaluate('document.getElementById("summary-section").checkVisibility()'), true);
  console.log('PASS published address edits require sign-in, cancel preserves draft, failure keeps old addresses, success updates and saves together');
  await click('remove-discovery');
  await fill('#navidrome-admin', 'admin'); await fill('#navidrome-password', 'fake-nav-password');
  await submit('discovery-form'); await idle();
  assert.equal(discovery.status, 'manual');
  assert.equal(discovery.hasAnnouncement, false);
  console.log('PASS explicit publish/remove refreshes status and discards form credentials');
  saved.settings.listeners = beforeAddresses;
  await cdp('Page.reload'); await idle();
  await until('document.getElementById("summary-section").checkVisibility()');
  await click('nav-settings');
  await shot('connections-saved');
  await click('cache-clear');
  assert.equal(await evaluate('document.getElementById("cache-confirmation").hidden'), false);
  await click('cache-cancel');
  assert.equal(storageRequests, 0);
  assert.equal(storage.cache.retained_bytes, 123e6);
  await fill('#cache-enabled', 'false'); await submit('storage-form');
  assert.match(await evaluate('document.getElementById("cache-confirm-title").textContent'), /Turn off caching and clear/);
  await shot('cache-disable-confirmation');
  await click('cache-cancel');
  assert.equal(await evaluate('document.getElementById("cache-enabled").value'), 'true');
  assert.equal(storageRequests, 0);
  console.log('PASS clear/disable cancellation preserves the cache and saved settings');
  await fill('#cache-enabled', 'false'); await submit('storage-form');
  await click('cache-confirm'); await idle();
  assert.equal(storageRequests, 1);
  assert.equal(storage.settings.enabled, false);
  assert.equal(storage.cache.retained_bytes, 0);
  assert.equal(storage.working_bytes, 64e6);
  console.log('PASS disabling clears retention with one confirmation and keeps working use separate');
  await fill('#cache-enabled', 'true'); await fill('#cache-limit', '1000'); await submit('storage-form'); await idle();
  assert.equal(storage.settings.limit_bytes, 1000e9);
  assert.equal(storage.settings.enabled, true);
  errorNext = { status: 503, message: 'Could not save cache settings.' };
  await fill('#cache-limit', '1'); await submit('storage-form'); await idle();
  assert.equal(await evaluate('document.getElementById("cache-limit").value'), '1000');
  assert.equal(storage.settings.limit_bytes, 1000e9);
  console.log('PASS custom cache size has no arbitrary tier cap and a failed save restores saved values');
  storage.cache.retained_bytes = 456e6;
  await click('storage-refresh'); await idle();
  const beforeClear = storageRequests;
  await click('cache-clear');
  assert.equal(storageRequests, beforeClear);
  await click('cache-confirm'); await idle();
  assert.equal(storageRequests, beforeClear + 1);
  assert.equal(storage.cache.retained_bytes, 0);
  assert.match(await evaluate('document.getElementById("message").textContent'), /cleared/);
  console.log('PASS explicit clear updates usage only after confirmation');
  await click('cache-clear');
  await evaluate('document.getElementById("cache-limit").dispatchEvent(new Event("input", { bubbles: true }))');
  assert.equal(await evaluate('document.getElementById("cache-confirmation").hidden'), true);
  storage.settings.revision = 'changed-by-cli';
  await fill('#cache-limit', '10'); await submit('storage-form'); await idle();
  assert.equal(await evaluate('document.getElementById("reload").hidden'), false);
  await click('reload'); await idle();
  assert.equal(await evaluate('document.getElementById("cache-limit").value'), '1000');
  console.log('PASS edited drafts cancel stale confirmations and concurrent changes require reload');
  await shot('cache-controls');
  assert.equal(await evaluate('document.getElementById("resource-conversions").value'),'automatic');
  assert.match(await evaluate('document.getElementById("resources-status").textContent'),/limit 2 \(Automatic\)/);
  const cacheRevision = storage.settings.revision;
  await fill('#resource-conversions','1'); await fill('#resource-working','128');
  await submit('resources-form'); await idle();
  assert.equal(storage.resources.max_conversions,1);
  assert.equal(storage.resources.working_bytes,128e6);
  assert.equal(storage.settings.revision,cacheRevision);
  assert.match(await evaluate('document.getElementById("storage-reserved").textContent'),/reserved/);
  console.log('PASS resource changes save separately and show working reservations');
  errorNext={status:503,message:'Could not save resource settings.'};
  await fill('#resource-conversions','4'); await fill('#resource-working','512');
  await submit('resources-form'); await idle();
  assert.equal(await evaluate('document.getElementById("resource-conversions").value'),'1');
  assert.equal(await evaluate('document.getElementById("resource-working").value'),'128');
  console.log('PASS failed resource save restores both saved choices');
  storage.resources.revision='changed-by-cli';
  await fill('#resource-conversions','automatic');await submit('resources-form');await idle();
  assert.equal(await evaluate('document.getElementById("reload").hidden'),false);
  await click('reload');await idle();
  await fill('#resource-conversions','automatic');await submit('resources-form');await idle();
  assert.equal(storage.resources.max_conversions,null);
  assert.equal(storage.runtime.conversions.limit,2);
  console.log('PASS stale resource updates require reload and Automatic can be restored');
  await shot('resource-controls');

  storage.settings.limit_bytes = 5e9; storage.resources.working_bytes = 512e6;
  activity = { clients: [
    {id:1, streams:[{id:2,quality:'lossless',sent_seconds_ago:1},{id:3,quality:null,sent_seconds_ago:null}]},
    {id:4, streams:[]},
  ] };
  rate = 234567;
  storage.runtime.conversions.running = 1;
  storage.runtime.conversions.waiting = 2;
  await click('nav-overview');
  await until('document.getElementById("live-clients").textContent === "2"');
  assert.equal(await evaluate('document.getElementById("live-streams").textContent'),'2');
  assert.equal(await evaluate('document.getElementById("live-rate").textContent'),'234.6 kB/s');
  assert.match(await evaluate('document.getElementById("live-list").textContent'),/Original \(lossless\).*Prepared; no audio sent yet.*No open stream/s);
  console.log('PASS live activity distinguishes sessions, prepared streams and last delivered quality');
  await click('nav-settings');
  await fill('#cache-limit','42'); await fill('#resource-conversions','3'); await fill('#resource-working','384');
  await click('storage-refresh'); await idle();
  assert.equal(await evaluate('document.getElementById("cache-limit").value'),'42');
  assert.equal(await evaluate('document.getElementById("resource-working").value'),'384');
  console.log('PASS Refresh usage keeps unsaved settings and their revision guards');
  const settingsCount = dashboardRequests;
  await pause(2200);
  assert.equal(dashboardRequests,settingsCount);
  await click('nav-overview');
  activity.clients[0].streams[0].quality = 'opus-64';
  storage.cache.retained_bytes = 987e6;
  await until('document.getElementById("live-list").textContent.includes("Opus 64 kbps")');
  assert.equal(await evaluate('document.getElementById("cache-limit").value'),'42');
  assert.equal(await evaluate('document.getElementById("resource-conversions").value'),'3');
  assert.equal(await evaluate('document.getElementById("resource-working").value'),'384');
  assert.match(await evaluate('document.getElementById("storage-usage").textContent'),/987 MB/);
  assert.equal(await evaluate('document.getElementById("overview-cache-meter").getAttribute("aria-valuetext").includes("987 MB")'),true);
  console.log('PASS Settings pauses polling; switching views preserves drafts and updates usage meters');
  await click('nav-settings');
  assert.equal(await evaluate('document.getElementById("cache-limit").value'),'42');
  await click('cache-clear');
  await click('nav-overview');
  const beforePoll = dashboardRequests;
  await until(`document.getElementById("dashboard-status").textContent.startsWith("Live ·")`);
  while (dashboardRequests === beforePoll) await pause(20);
  assert.equal(await evaluate('document.getElementById("cache-confirmation").hidden'),false);
  await click('nav-settings');
  await click('cache-cancel');
  await click('nav-overview');
  console.log('PASS live polling preserves the cache confirmation');
  dashboardError = {status:503,message:'Temporary read failure.'};
  await until('document.getElementById("dashboard-status").textContent.includes("Out of date")');
  assert.equal(await evaluate('document.getElementById("live-clients").textContent'),'2');
  await until('document.getElementById("dashboard-status").textContent.startsWith("Live ·")');
  console.log('PASS failed polls mark stale values and recover without replacing them with zero');
  await evaluate('Object.defineProperty(document,"hidden",{configurable:true,get:()=>true}); document.dispatchEvent(new Event("visibilitychange"))');
  const hiddenCount = dashboardRequests;
  await pause(2200);
  assert.equal(dashboardRequests,hiddenCount);
  await evaluate('delete document.hidden; document.dispatchEvent(new Event("visibilitychange"))');
  await until('document.getElementById("dashboard-status").textContent.startsWith("Live ·")');
  console.log('PASS hidden-tab polling pauses and resumes');
  await cdp('Emulation.setEmulatedMedia',{features:[{name:'prefers-color-scheme',value:'dark'},{name:'prefers-reduced-motion',value:'reduce'}]});
  await shot('dashboard-dark');
  await cdp('Emulation.setEmulatedMedia',{features:[{name:'prefers-color-scheme',value:'light'},{name:'prefers-reduced-motion',value:'reduce'}]});
  await shot('dashboard-light');
  discovery.status = 'update-needed'; discovery.hasAnnouncement = true;
  await click('nav-settings'); await cdp('Page.reload'); await until('typeof busy !== "undefined" && !busy && document.getElementById("summary-section").checkVisibility()');
  await click('nav-overview');
  assert.equal(await evaluate('document.getElementById("overview-discovery-notice").checkVisibility()'),true);
  await click('discovery-settings');
  assert.equal(await evaluate('document.activeElement.id'),'discovery-heading');
  await click('publish-discovery'); await fill('#navidrome-admin','admin'); await fill('#navidrome-password','discard this');
  await click('nav-overview'); await click('nav-settings');
  assert.equal(await evaluate('document.getElementById("navidrome-password").value'),'');
  assert.equal(await evaluate('document.getElementById("discovery-form").hidden'),true);
  console.log('PASS discovery attention is visible from Overview and leaving Settings clears credentials');
  discovery.status = 'manual'; discovery.hasAnnouncement = false;
  await cdp('Page.reload'); await until('typeof busy !== "undefined" && !busy && document.getElementById("summary-section").checkVisibility()');
  await click('nav-overview');

  health = {status:'problem',checked_seconds_ago:3,message:'Navidrome refused the connection. Check its address and port.'};
  await until('document.getElementById("live-connection-error").checkVisibility()');
  await shot('dashboard-problem');
  health = {status:'responding',checked_seconds_ago:1};
  await until('document.getElementById("live-navidrome").textContent === "Online"');
  assert.equal(await evaluate('document.getElementById("live-connection-error").hidden'),true);
  console.log('PASS connection problems expand and successful recovery becomes compact');
  storage.cache.retained_bytes = 6e9; storage.settings.limit_bytes = 5e9;
  storage.runtime.working.reserved_bytes = 640e6;
  await until('document.getElementById("overview-cache-meter").dataset.over === "true"');
  assert.equal(await evaluate('document.getElementById("overview-cache-meter").getAttribute("aria-valuenow")'),'100');
  assert.equal(await evaluate('document.querySelector("#overview-working-meter .meter-reserved").style.width'),'100%');
  assert.match(await evaluate('document.getElementById("overview-cache-usage").textContent'),/6 GB/);
  console.log('PASS above-limit storage keeps true readings with bounded visual bars');
  await click('manage-storage');
  assert.equal(await evaluate('document.getElementById("settings-panel").checkVisibility()'),true);
  assert.equal(await evaluate('document.activeElement.id'),'storage-heading');
  await click('edit-listeners');
  await fill('#addresses input', 'https://stale.lan:7843');
  saved.revision = 'other-tab';
  await submit('listeners-form'); await idle();
  assert.equal(await evaluate('document.getElementById("reload").hidden'), false);
  assert.equal(await evaluate('document.querySelector("#addresses input").value'), 'https://stale.lan:7843');
  assert.equal(saved.settings.listeners[0].url, 'https://home.lan:7843');
  await click('reload'); await idle();
  assert.equal(await evaluate('document.getElementById("summary-section").hidden'), false);
  console.log('PASS conflicting save keeps draft until explicit reload');
  await cdp('Emulation.setDeviceMetricsOverride', { width: 400, height: 900, deviceScaleFactor: 1, mobile: false });
  await shot('settings-narrow');
  await click('edit-listeners');
  await shot('connections-narrow');
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true);
  await click('back-navidrome');
  await click('nav-overview');
  await shot('overview-narrow');
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true);
  await click('nav-settings');
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true);
  await click('nav-overview');
  holdDashboard = true;
  dashboardError = {status:401,message:'An old sign-in expired.'};
  await evaluate('startDashboard()');
  for (let tries=0; !releaseDashboard && tries<100; tries++) await pause(20);
  assert.ok(releaseDashboard);
  await click('logout'); await idle();
  assert.equal(await evaluate('document.getElementById("owner-form").hidden'), false);
  const signedOutCount = dashboardRequests;
  await pause(2200);
  assert.equal(dashboardRequests,signedOutCount);
  await fill('#password', 'disposable fake password'); await submit('owner-form'); await idle();
  releaseDashboard();
  await pause(100);
  assert.equal(await evaluate('document.getElementById("summary-section").hidden'), false);
  assert.equal(await evaluate('document.getElementById("password").value'), '');
  console.log('PASS sign-out stops polling and a late expired response cannot sign out a new session');
  console.log('PASS sign-out/sign-in resumes saved setup and clears password input');
  assert.deepEqual(await evaluate('policyViolations'), []);
  console.log('PASS owner content-security policy permits the UI and dynamic storage meters');
  assert.deepEqual(errors, []);
  console.log('PASS no browser script errors or narrow-width overflow');
} finally {
  clearTimeout(watchdog);
  stop();
  for (const item of pending.values()) clearTimeout(item.timeout);
  if (browser && browser.exitCode === null) await new Promise((resolve) => { browser.once('exit', resolve); setTimeout(resolve, 2000).unref(); });
  await rm(profile, { recursive: true, force: true });
}
