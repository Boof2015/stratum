'use strict';
const element = (id) => document.getElementById(id);
let needsOwner = false;
let busy = false;
let saved = null;
let discoveryState = { revision: 'initial', status: 'not-published', hasAnnouncement: false };
let discoveryAction = null;
let storageState = null;
let pendingStorage = null;

async function request(path, body) {
  const response = await fetch(`/admin/api/${path}`, body === undefined ? {} : {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  if (!response.ok) {
    const error = new Error(value.error || 'Sidecar could not complete the request. Try again.');
    if (value.httpStatus) error.message += ` (HTTP ${value.httpStatus})`;
    error.status = response.status;
    throw error;
  }
  return value;
}
function controls() {
  document.querySelectorAll('button, input, select').forEach((control) => { control.disabled = busy; });
  element('add-address').disabled = busy || element('addresses').children.length >= 16;
  document.querySelectorAll('.remove-address').forEach((button) => {
    button.disabled = busy || element('addresses').children.length === 1;
  });
}
async function operate(action) {
  if (busy) return;
  busy = true;
  element('message').textContent = '';
  element('reload').hidden = true;
  controls();
  try { await action(); }
  catch (error) {
    element('message').textContent = error.message;
    if (error.status === 409) element('reload').hidden = false;
    if (error.status === 401) {
      saved = null;
      await refresh().catch(() => { element('retry').hidden = false; });
    }
  } finally { busy = false; controls(); }
}
function showStep(step) {
  element('owner-form').hidden = true;
  element('ready').hidden = false;
  for (const name of ['navidrome', 'listeners', 'summary']) element(`${name}-section`).hidden = step !== name;
  const headings = {
    navidrome: ['Connect Navidrome', 'Enter the address Sidecar can reach, including any path such as /music.'],
    listeners: ['How will listeners reach Sidecar?', 'Add the addresses you plan to use in Astra. The first is the primary address.'],
    summary: ['Connections saved', 'You can return here to change or check these settings.'],
  };
  element('title').textContent = headings[step][0];
  element('description').textContent = headings[step][1];
  if (step === 'navidrome') element('navidrome-url').value = saved.settings.navidrome_url || '';
  if (step === 'listeners') {
    element('addresses').replaceChildren();
    const listeners = saved.settings.listeners;
    for (const endpoint of listeners.length ? listeners : [{ url: '', trust: 'sidecar_key' }]) addAddress(endpoint);
  }
  clearDiscoveryForm();
  cancelStorage();
  if (step === 'summary') {
    renderStorage();
    renderDiscovery();
    element('saved-navidrome').textContent = saved.settings.navidrome_url;
    element('saved-addresses').replaceChildren();
    saved.settings.listeners.forEach((endpoint, index) => {
      const row = document.createElement('li');
      row.textContent = `${index === 0 ? 'Primary: ' : ''}${endpoint.url} — ${endpoint.trust === 'sidecar_key' ? 'Direct to Sidecar' : 'HTTPS reverse proxy'}`;
      element('saved-addresses').append(row);
    });
  }
  controls();
}
function resume(value) {
  saved = value.setup;
  if (value.storage) storageState = value.storage;
  if (value.discovery) discoveryState = value.discovery;
  else if (discoveryState.hasAnnouncement) discoveryState = { ...discoveryState, status: 'update-needed' };
  showStep(value.stage === 'connections-saved' ? 'summary' : value.stage);
}
function addAddress(endpoint = { url: '', trust: 'sidecar_key' }) {
  const row = document.createElement('fieldset');
  row.className = 'address-row';
  const legend = document.createElement('legend');
  const label = document.createElement('label');
  label.textContent = 'Listener address';
  const input = document.createElement('input');
  input.type = 'url'; input.required = true; input.maxLength = 1024; input.value = endpoint.url;
  input.placeholder = 'https://sidecar.home.lan:7843'; input.autocomplete = 'off'; input.spellcheck = false;
  label.append(input);
  const modeLabel = document.createElement('label');
  modeLabel.textContent = 'Connection';
  const select = document.createElement('select');
  for (const [value, text] of [['sidecar_key', 'Direct to Sidecar'], ['https_proxy', 'Through an HTTPS reverse proxy']]) {
    const option = document.createElement('option'); option.value = value; option.textContent = text; select.append(option);
  }
  select.value = endpoint.trust;
  modeLabel.append(select);
  const remove = document.createElement('button');
  remove.type = 'button'; remove.className = 'remove-address'; remove.textContent = 'Remove address';
  remove.addEventListener('click', () => { row.remove(); numberAddresses(); controls(); });
  row.append(legend, label, modeLabel, remove);
  element('addresses').append(row);
  numberAddresses(); controls();
}
function numberAddresses() {
  [...element('addresses').children].forEach((row, index) => {
    row.querySelector('legend').textContent = index === 0 ? 'Primary address' : `Additional address ${index}`;
  });
}
async function refresh() {
  element('retry').hidden = true;
  element('owner-form').hidden = true;
  element('ready').hidden = true;
  clearDiscoveryForm();
  try {
    const status = await request('status');
    needsOwner = !status.ownerConfigured;
    if (!needsOwner) {
      try { resume(await request('setup')); return; }
      catch (error) { if (error.status !== 401) throw error; }
    }
    saved = null;
    storageState = null;
    cancelStorage();
    element('title').textContent = needsOwner ? 'Make Sidecar yours' : 'Welcome back';
    element('description').textContent = needsOwner
      ? 'Enter the setup code from your server’s terminal or container logs, then choose a password of at least 12 characters.'
      : 'Sign in with your Sidecar administration password.';
    element('code-label').hidden = !needsOwner; element('code').required = needsOwner;
    element('confirm-label').hidden = !needsOwner; element('confirm').required = needsOwner;
    element('password').autocomplete = needsOwner ? 'new-password' : 'current-password';
    element('submit').textContent = needsOwner ? 'Create owner password' : 'Sign in';
    element('owner-form').hidden = false;
  } catch (error) {
    element('title').textContent = 'Couldn’t open Sidecar';
    element('retry').hidden = false;
    throw error;
  }
}
element('owner-form').addEventListener('submit', (event) => {
  event.preventDefault();
  operate(async () => {
    const password = element('password').value;
    if (needsOwner && password !== element('confirm').value) throw new Error('The passwords don’t match.');
    try {
      await request(needsOwner ? 'claim' : 'login', needsOwner ? { password, code: element('code').value.trim() } : { password });
      element('owner-form').reset();
      await refresh();
    } finally { element('password').value = ''; element('confirm').value = ''; }
  });
});
element('navidrome-form').addEventListener('submit', (event) => {
  event.preventDefault();
  operate(async () => {
    element('navidrome-save').textContent = 'Checking…';
    try {
      const value = await request('setup/navidrome', { revision: saved.revision, url: element('navidrome-url').value });
      saved = value.setup;
      if (discoveryState.hasAnnouncement) discoveryState = { ...discoveryState, status: 'update-needed' };
      showStep(saved.settings.listeners.length ? 'summary' : 'listeners');
      element('message').textContent = 'Navidrome detected. Sign-in has not been checked.';
    } finally { element('navidrome-save').textContent = 'Check and continue'; }
  });
});
element('listeners-form').addEventListener('submit', (event) => {
  event.preventDefault();
  operate(async () => {
    const listeners = [...element('addresses').children].map((row) => ({
      url: row.querySelector('input').value, trust: row.querySelector('select').value,
    }));
    resume(await request('setup/listeners', { revision: saved.revision, listeners }));
  });
});
element('add-address').addEventListener('click', () => { addAddress(); element('addresses').lastElementChild.querySelector('input').focus(); });
for (const [id, step] of [['edit-navidrome', 'navidrome'], ['back-navidrome', 'navidrome'], ['edit-listeners', 'listeners']]) {
  element(id).addEventListener('click', () => { element('message').textContent = ''; element('reload').hidden = true; showStep(step); });
}
element('check-again').addEventListener('click', () => operate(async () => {
  await request('setup/check', { url: saved.settings.navidrome_url });
  element('message').textContent = 'Navidrome detected. Sign-in has not been checked.';
}));
element('logout').addEventListener('click', () => operate(async () => { await request('logout', {}); saved = null; await refresh(); }));
for (const id of ['retry', 'reload']) element(id).addEventListener('click', () => operate(refresh));
function clearDiscoveryForm() {
  discoveryAction = null;
  element('discovery-form').reset();
  element('discovery-form').hidden = true;
}
function renderDiscovery() {
  const labels = {
    'not-published': 'Choose automatic discovery or manual address setup.',
    manual: 'Manual address setup selected. No announcement is published.',
    published: 'Announcement published and verified on Navidrome.',
    'update-needed': 'The announcement needs updating to match the saved connections.',
    unverified: 'An earlier change needs verification. Retry to check what reached Navidrome.',
    problem: 'Discovery needs attention.',
  };
  element('discovery-status').textContent = labels[discoveryState.status] || labels['not-published'];
  element('discovery-error').textContent = discoveryState.error || '';
  element('publish-discovery').hidden = discoveryState.pendingAction === 'remove';
  element('publish-discovery').textContent = discoveryState.canRecreate ? 'Recreate announcement' : discoveryState.hasAnnouncement ? 'Update or verify discovery' : discoveryState.error ? 'Retry publication' : 'Publish discovery';
  element('manual-discovery').hidden = discoveryState.hasAnnouncement || discoveryState.status === 'manual';
  element('remove-discovery').hidden = !discoveryState.hasAnnouncement;
  element('remove-discovery').textContent = discoveryState.pendingAction === 'remove' ? 'Retry removal' : 'Remove announcement';
}
function showDiscoveryForm(action) {
  clearDiscoveryForm(); discoveryAction = action;
  const target = action === 'remove' ? discoveryState.target : saved.settings.navidrome_url;
  element('discovery-target').textContent = `Sign in to Navidrome at ${target}`;
  element('discovery-submit').textContent = action === 'remove' ? 'Remove announcement' : discoveryState.canRecreate ? 'Recreate announcement' : 'Publish discovery';
  element('discovery-form').hidden = false;
  element('navidrome-admin').focus();
}
for (const [id, action] of [['publish-discovery', 'publish'], ['remove-discovery', 'remove']]) {
  element(id).addEventListener('click', () => showDiscoveryForm(action));
}
element('discovery-cancel').addEventListener('click', clearDiscoveryForm);
element('manual-discovery').addEventListener('click', () => operate(async () => {
  resume(await request('discovery', { action: 'manual', setup_revision: saved.revision, discovery_revision: discoveryState.revision }));
}));
element('discovery-form').addEventListener('submit', (event) => {
  event.preventDefault();
  operate(async () => {
    const body = { action: discoveryAction, setup_revision: saved.revision, discovery_revision: discoveryState.revision,
      username: element('navidrome-admin').value, password: element('navidrome-password').value,
      recreate: discoveryAction === 'publish' && !!discoveryState.canRecreate };
    // Clear DOM values as soon as the request begins, including failed requests.
    element('discovery-form').reset();
    try { resume(await request('discovery', body)); }
    catch (error) {
      // The server may have saved an interrupted/failed operation. Reload its
      // public state so Retry can reconcile it with fresh credentials.
      if (error.status !== 401) {
        try { resume(await request('setup')); } catch { /* Keep the original error. */ }
      }
      throw error;
    } finally { body.password = ''; body.username = ''; }
  });
});
function storageBytes(value) {
  return `${(value / 1e9).toLocaleString(undefined, { maximumFractionDigits: 3 })} GB`;
}
function cancelStorage() {
  pendingStorage = null;
  element('cache-confirmation').hidden = true;
}
function renderStorage() {
  element('storage-form').hidden = !storageState;
  element('resources-form').hidden = !storageState?.resources;
  if (!storageState) return;
  const { settings, cache, working_bytes: working } = storageState;
  element('storage-usage').textContent = `Cached audio: ${storageBytes(cache.retained_bytes)} / ${storageBytes(settings.limit_bytes)}`;
  element('storage-working').textContent = `Temporary storage for active streams: ${storageBytes(working)}`;
  element('storage-problem').textContent = cache.problem
    ? 'Cache changes are waiting for storage access. Sidecar will retry; playback can continue without retention.' : '';
  element('cache-enabled').value = String(settings.enabled);
  element('cache-limit').value = String(settings.limit_bytes / 1e9);
  if (storageState.resources) {
    const { resources, runtime } = storageState;
    element('resource-conversions').value = resources.max_conversions === null ? 'automatic' : String(resources.max_conversions);
    element('resource-working').value = String(resources.working_bytes / 1e6);
    const jobs = runtime.conversions;
    element('resources-status').textContent = `${jobs.running} running · ${jobs.waiting} waiting · limit ${jobs.limit}${resources.max_conversions === null ? ' (Automatic)' : ''}`;
    element('storage-working').textContent = `Temporary storage: ${storageBytes(working)} used · ${storageBytes(runtime.working.reserved_bytes)} reserved / ${storageBytes(resources.working_bytes)} limit`;
  }
}
function confirmStorage(body) {
  pendingStorage = body;
  const disabling = !body.enabled && storageState.settings.enabled;
  element('cache-confirm-title').textContent = disabling ? 'Turn off caching and clear saved audio?' : 'Clear cached audio?';
  element('cache-confirm-message').textContent = disabling
    ? 'This removes the retained cache and stops saving new audio. Active playback keeps the temporary files it needs until it finishes.'
    : body.enabled ? 'This removes retained audio. Active playback keeps its temporary files, and future listening can fill the cache again.'
    : 'This removes retained audio. Caching stays off. Active playback keeps its temporary files.';
  element('cache-confirm').textContent = disabling ? 'Turn off and clear' : 'Clear cached audio';
  element('cache-confirmation').hidden = false;
  element('cache-cancel').focus();
}
async function saveStorage(body) {
  try {
    storageState = await request('storage', body);
    cancelStorage();
    element('message').textContent = storageState.cache.problem
      ? 'Settings saved. Cache changes will be retried when storage is accessible.'
      : body.clear ? 'Retained audio cleared. Active playback can continue.'
      : !body.enabled ? 'Caching is off and retained audio is cleared.' : 'Cache settings saved.';
  } finally { renderStorage(); }
}
for (const event of ['input', 'change']) element('storage-form').addEventListener(event, cancelStorage);
element('storage-form').addEventListener('submit', (event) => {
  event.preventDefault();
  if (busy || !storageState) return;
  const limit = Number(element('cache-limit').value) * 1e9;
  if (!Number.isSafeInteger(limit) || limit < 1e9) {
    element('message').textContent = 'Use a cache limit of at least 1 GB, within the supported numeric range.';
    return;
  }
  const body = { revision: storageState.settings.revision, enabled: element('cache-enabled').value === 'true', limit_bytes: limit };
  if (storageState.settings.enabled && !body.enabled) confirmStorage(body);
  else operate(() => saveStorage(body));
});
element('cache-clear').addEventListener('click', () => {
  if (!busy && storageState) confirmStorage({ revision: storageState.settings.revision,
    enabled: storageState.settings.enabled, limit_bytes: storageState.settings.limit_bytes, clear: true });
});
element('cache-cancel').addEventListener('click', () => { cancelStorage(); renderStorage(); element('cache-clear').focus(); });
element('cache-confirm').addEventListener('click', () => {
  if (pendingStorage) operate(() => saveStorage({ ...pendingStorage, confirmed: true }));
});
element('storage-refresh').addEventListener('click', () => operate(async () => {
  storageState = await request('storage'); cancelStorage(); renderStorage();
}));
element('resources-form').addEventListener('submit', (event) => {
  event.preventDefault();
  if (busy || !storageState?.resources) return;
  const working = Number(element('resource-working').value) * 1e6;
  if (!Number.isSafeInteger(working) || working < 128e6) {
    element('message').textContent = 'Use a working-storage limit of at least 128 MB, within the supported numeric range.';
    return;
  }
  const mode = element('resource-conversions').value;
  const body = { revision: storageState.resources.revision,
    max_conversions: mode === 'automatic' ? null : Number(mode), working_bytes: working };
  operate(async () => {
    try {
      storageState = await request('resources', body);
      cancelStorage();
      element('message').textContent = 'Resource settings saved. Current playback and jobs are protected.';
    } finally { renderStorage(); }
  });
});
operate(refresh);
