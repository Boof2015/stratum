'use strict';
const element = (id) => document.getElementById(id);
let needsOwner = false;
let busy = false;
let saved = null;
let discoveryState = { revision: 'initial', status: 'not-published', hasAnnouncement: false };
let discoveryAction = null;
let pendingListeners = null;
let storageState = null;
let pendingStorage = null;
let dashboardWanted = false;
let dashboardEpoch = 0;
let dashboardTimer = null;
let dashboardAbort = null;
let currentView = 'overview';
let currentStep = null;
let editingConnection = false;

async function request(path, body, signal) {
  const options = body === undefined ? {} : {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
  };
  const response = await fetch(`/admin/api/${path}`, { ...options, signal, cache: 'no-store' });
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
    button.disabled = busy;
    button.hidden = element('addresses').children.length === 1;
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
function selectView(view, focus = false) {
  if (currentView === 'settings' && view !== 'settings') clearDiscoveryForm();
  if (currentView !== view) { element('message').textContent = ''; element('reload').hidden = true; }
  currentView = view;
  for (const name of ['overview', 'settings']) {
    element(`${name}-panel`).hidden = name !== view;
    if (name === view) element(`nav-${name}`).setAttribute('aria-current', 'page');
    else element(`nav-${name}`).removeAttribute('aria-current');
  }
  element('title').textContent = view === 'overview' ? 'Overview' : 'Settings';
  element('description').textContent = '';
  dashboardWanted = currentStep === 'summary' && view === 'overview';
  stopDashboard();
  if (dashboardWanted && !document.hidden) startDashboard();
  if (focus) element('title').focus();
}
function showStep(step) {
  stopDashboard();
  dashboardWanted = false;
  currentStep = step;
  element('owner-form').hidden = true;
  element('ready').hidden = false;
  element('logout').hidden = false;
  element('app').classList.toggle('setup-layout', step !== 'summary');
  element('page-nav').hidden = step !== 'summary';
  element('setup-progress').hidden = step === 'summary' || editingConnection;
  const steps = ['navidrome', 'listeners', 'discovery'];
  document.querySelectorAll('[data-step]').forEach((item, index) => {
    const active = item.dataset.step === step;
    if (active) item.setAttribute('aria-current', 'step');
    else item.removeAttribute('aria-current');
    const done = index < steps.indexOf(step);
    item.classList.toggle('complete', done);
    item.querySelector('.step-number').textContent = done ? '✓' : String(index + 1);
  });
  for (const name of ['navidrome', 'listeners', 'discovery', 'summary']) element(`${name}-section`).hidden = step !== name;
  element(step === 'discovery' ? 'discovery-section' : 'discovery-settings-slot').append(element('discovery-panel'));
  element('discovery-back-row').hidden = step !== 'discovery';
  element('discovery-heading').hidden = step === 'discovery';
  const headings = {
    navidrome: ['Connect Navidrome', ''],
    listeners: ['Listener connections', ''],
    discovery: ['Discovery', ''],
  };
  if (headings[step]) {
    element('title').textContent = headings[step][0];
    element('description').textContent = headings[step][1];
  }
  element('cancel-navidrome').hidden = !editingConnection;
  element('navidrome-save').textContent = editingConnection ? 'Check and save' : 'Check and continue';
  element('back-navidrome').textContent = editingConnection ? 'Cancel' : 'Back';
  element('listeners-save').textContent = editingConnection ? 'Save changes' : 'Continue';
  if (step === 'navidrome') element('navidrome-url').value = saved.settings.navidrome_url || '';
  if (step === 'listeners') {
    element('addresses').replaceChildren();
    const listeners = saved.settings.listeners;
    for (const endpoint of listeners.length ? listeners : [{ url: '', trust: 'sidecar_key' }]) addAddress(endpoint);
  }
  if (editingConnection && step === 'navidrome') element('title').textContent = 'Change Navidrome address';
  if (editingConnection && step === 'listeners') element('title').textContent = 'Listener connections';
  clearDiscoveryForm();
  cancelStorage();
  renderDiscovery();
  if (step === 'summary') {
    editingConnection = false;
    renderStorage();
    element('saved-navidrome').textContent = saved.settings.navidrome_url;
    element('saved-addresses').replaceChildren();
    saved.settings.listeners.forEach((endpoint, index) => {
      const row = document.createElement('li');
      row.append(document.createTextNode(endpoint.url));
      const note = document.createElement('span'); note.className = 'hint';
      note.textContent = `${index === 0 ? 'Primary · ' : ''}${endpoint.trust === 'sidecar_key' ? 'Direct to Sidecar' : 'Reverse proxy'}`;
      row.append(note); element('saved-addresses').append(row);
    });
    selectView(currentView);
  }
  controls();
  element('title').focus();
}
function resume(value) {
  saved = value.setup;
  if (value.storage) storageState = value.storage;
  if (value.discovery) discoveryState = value.discovery;
  else if (discoveryState.hasAnnouncement) discoveryState = { ...discoveryState, status: 'update-needed' };
  let step = value.stage;
  if (step === 'connections-saved') {
    const discoveryDone = ['manual', 'published'].includes(discoveryState.status);
    const needsDiscovery = !discoveryDone && (!discoveryState.hasAnnouncement || currentStep === 'discovery');
    step = !editingConnection && needsDiscovery ? 'discovery' : 'summary';
  }
  showStep(step);
}
function addAddress(endpoint = { url: '', trust: 'sidecar_key' }) {
  const row = document.createElement('fieldset'); row.className = 'address-row';
  const legend = document.createElement('legend');
  const choiceLabel = document.createElement('span'); choiceLabel.className = 'field-caption'; choiceLabel.textContent = 'Connection';
  const modes = document.createElement('div'); modes.className = 'connection-choice'; modes.setAttribute('role', 'group'); modes.setAttribute('aria-label', 'Connection type');
  const select = document.createElement('select'); select.hidden = true; select.setAttribute('aria-label', 'Connection type');
  const hint = document.createElement('p'); hint.className = 'hint connection-hint';
  const label = document.createElement('label'); label.textContent = 'Listener address';
  const input = document.createElement('input');
  input.type = 'url'; input.required = true; input.maxLength = 1024; input.value = endpoint.url;
  input.autocomplete = 'off'; input.spellcheck = false; label.append(input);
  const pathHint = document.createElement('span'); pathHint.className = 'hint'; pathHint.textContent = 'Include the path your proxy uses, such as /stratum.'; label.append(pathHint);
  function updateMode() {
    const proxy = select.value === 'https_proxy';
    for (const button of modes.children) button.setAttribute('aria-pressed', String(button.dataset.mode === select.value));
    hint.textContent = proxy ? 'Your proxy handles the HTTPS connection.' : 'Listeners connect straight to Sidecar.';
    input.placeholder = proxy ? 'https://music.example.com/stratum' : 'https://sidecar.home.lan:7843';
    pathHint.hidden = !proxy;
  }
  for (const [value, text] of [['sidecar_key', 'Direct to Sidecar'], ['https_proxy', 'Reverse proxy']]) {
    const option = document.createElement('option'); option.value = value; option.textContent = text; select.append(option);
    const button = document.createElement('button'); button.type = 'button'; button.textContent = text; button.dataset.mode = value;
    button.addEventListener('click', () => { select.value = value; updateMode(); }); modes.append(button);
  }
  select.value = endpoint.trust; select.addEventListener('change', updateMode); updateMode();
  const remove = document.createElement('button');
  remove.type = 'button'; remove.className = 'remove-address quiet'; remove.textContent = 'Remove';
  remove.addEventListener('click', () => {
    const previous = row.previousElementSibling || row.nextElementSibling;
    row.remove(); numberAddresses(); controls(); previous?.querySelector('input').focus();
  });
  row.append(legend, choiceLabel, modes, hint, label, select, remove);
  element('addresses').append(row); numberAddresses(); controls();
}
function numberAddresses() {
  [...element('addresses').children].forEach((row, index) => {
    row.querySelector('legend').textContent = index === 0 ? 'Primary address' : `Additional address ${index}`;
  });
}
async function refresh() {
  dashboardWanted = false;
  stopDashboard();
  element('retry').hidden = true;
  element('owner-form').hidden = true;
  element('ready').hidden = true;
  element('page-nav').hidden = true;
  element('setup-progress').hidden = true;
  element('logout').hidden = true;
  clearDiscoveryForm();
  try {
    const status = await request('status');
    needsOwner = !status.ownerConfigured;
    if (!needsOwner) {
      try { resume(await request('setup')); return; }
      catch (error) { if (error.status !== 401) throw error; }
    }
    saved = null;
    currentStep = null; currentView = 'overview'; editingConnection = false;
    element('app').classList.add('setup-layout');
    storageState = null;
    cancelStorage();
    element('title').textContent = needsOwner ? 'Set up Sidecar' : 'Sign in';
    element('description').textContent = '';
    element('password-help').hidden = !needsOwner;
    element('password').minLength = needsOwner ? 12 : 1;
    element('code-label').hidden = !needsOwner; element('code').required = needsOwner;
    element('confirm-label').hidden = !needsOwner; element('confirm').required = needsOwner;
    element('password').autocomplete = needsOwner ? 'new-password' : 'current-password';
    element('submit').textContent = needsOwner ? 'Set password' : 'Sign in';
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
      resume({ setup: saved, stage: saved.settings.listeners.length ? 'connections-saved' : 'listeners', discovery: discoveryState });
    } finally { element('navidrome-save').textContent = editingConnection ? 'Check and save' : 'Check and continue'; }
  });
});
element('listeners-form').addEventListener('submit', (event) => {
  event.preventDefault();
  if (busy) return;
  const listeners = [...element('addresses').children].map((row) => ({
    url: row.querySelector('input').value, trust: row.querySelector('select').value,
  }));
  const normalized = listeners.map((endpoint) => ({ ...endpoint, url: endpoint.url.trim().replace(/\/+$/, '') }));
  if (discoveryState.hasAnnouncement && (discoveryState.status !== 'published'
    || JSON.stringify(normalized) !== JSON.stringify(saved.settings.listeners))) {
    element('message').textContent = '';
    showDiscoveryForm('publish', listeners);
    return;
  }
  operate(async () => {
    resume(await request('setup/listeners', { revision: saved.revision, listeners }));
  });
});
element('add-address').addEventListener('click', () => { addAddress(); element('addresses').lastElementChild.querySelector('input').focus(); });
for (const [id, step] of [['edit-navidrome', 'navidrome'], ['edit-listeners', 'listeners']]) {
  element(id).addEventListener('click', () => {
    editingConnection = true; element('message').textContent = ''; element('reload').hidden = true;
    showStep(step); element('title').focus();
  });
}
for (const id of ['back-navidrome', 'cancel-navidrome']) element(id).addEventListener('click', () => {
  element('message').textContent = ''; element('reload').hidden = true;
  showStep(editingConnection ? 'summary' : 'navidrome'); element('title').focus();
});
element('back-listeners').addEventListener('click', () => { showStep('listeners'); element('title').focus(); });
for (const view of ['overview', 'settings']) element(`nav-${view}`).addEventListener('click', () => selectView(view, true));
element('brand').addEventListener('click', (event) => { event.preventDefault(); if (currentStep === 'summary' && !busy) selectView('overview', true); });
for (const [id, target] of [['manage-storage', 'storage-settings'], ['health-settings', 'connection-settings'], ['discovery-settings', 'discovery-panel']]) {
  element(id).addEventListener('click', () => {
    selectView('settings'); const heading = element(target).querySelector('h2');
    heading.tabIndex = -1; heading.focus(); heading.scrollIntoView({ block: 'start' });
  });
}
element('check-again').addEventListener('click', () => operate(async () => {
  await request('setup/check', { url: saved.settings.navidrome_url });
  element('message').textContent = 'Navidrome is reachable.';
}));
element('logout').addEventListener('click', () => operate(async () => {
  dashboardWanted = false; stopDashboard();
  await request('logout', {}); saved = null; await refresh();
}));
for (const id of ['retry', 'reload']) element(id).addEventListener('click', () => operate(refresh));
function clearDiscoveryForm() {
  if (pendingListeners) {
    element('discovery-back-row').before(element('discovery-form'));
    element('discovery-form').classList.remove('panel');
    element('listeners-form').hidden = false;
  }
  pendingListeners = null;
  discoveryAction = null;
  element('discovery-form').reset();
  element('discovery-form').hidden = true;
  element('discovery-actions').hidden = false;
}
function renderDiscovery() {
  const labels = {
    'not-published': 'Discovery is not enabled yet.',
    manual: 'Manual setup',
    published: 'Discovery enabled',
    'update-needed': 'The announcement needs updating to match the saved connections.',
    unverified: 'An earlier change needs verification. Retry to check what reached Navidrome.',
    problem: 'Discovery needs attention.',
  };
  element('discovery-status').textContent = labels[discoveryState.status] || labels['not-published'];
  const needsAttention = !!discoveryState.error || ['update-needed', 'unverified', 'problem'].includes(discoveryState.status);
  element('overview-discovery-notice').hidden = !needsAttention;
  element('overview-discovery-message').textContent = needsAttention ? labels[discoveryState.status] || 'Discovery needs attention.' : '';
  element('discovery-error').textContent = discoveryState.error || '';
  element('publish-discovery').hidden = discoveryState.pendingAction === 'remove';
  element('publish-discovery').textContent = discoveryState.canRecreate ? 'Recreate announcement' : discoveryState.hasAnnouncement ? 'Update discovery' : discoveryState.error ? 'Try again' : 'Enable discovery';
  element('manual-discovery').hidden = discoveryState.hasAnnouncement || discoveryState.status === 'manual';
  element('remove-discovery').hidden = !discoveryState.hasAnnouncement;
  element('remove-discovery').textContent = discoveryState.pendingAction === 'remove' ? 'Retry removal' : 'Remove announcement';
}
function showDiscoveryForm(action, listeners = null) {
  clearDiscoveryForm(); discoveryAction = action;
  pendingListeners = listeners;
  if (listeners) {
    element('listeners-section').append(element('discovery-form'));
    element('discovery-form').classList.add('panel');
    element('listeners-form').hidden = true;
  }
  const target = action === 'remove' ? discoveryState.target : saved.settings.navidrome_url;
  element('discovery-target').textContent = `Sign in to Navidrome at ${target}`;
  element('discovery-effect').textContent = listeners
    ? 'Clients need the new addresses to find Sidecar. Sign in to Navidrome to update discovery and save your changes.'
    : action === 'remove' ? 'Removes Sidecar’s entry from Navidrome’s internet radio list.'
    : 'Adds or updates a Stratum entry in Navidrome’s internet radio list. Other apps may show it as a radio station.';
  element('discovery-submit').textContent = listeners ? 'Update and save'
    : action === 'remove' ? 'Remove announcement' : discoveryState.canRecreate ? 'Recreate announcement'
    : discoveryState.hasAnnouncement ? 'Update discovery' : 'Enable discovery';
  element('discovery-actions').hidden = true;
  element('discovery-form').hidden = false;
  element('navidrome-admin').focus();
}
for (const [id, action] of [['publish-discovery', 'publish'], ['remove-discovery', 'remove']]) {
  element(id).addEventListener('click', () => showDiscoveryForm(action));
}
element('discovery-cancel').addEventListener('click', () => {
  const editing = !!pendingListeners;
  clearDiscoveryForm();
  element(editing ? 'listeners-save' : 'publish-discovery').focus();
});
element('manual-discovery').addEventListener('click', () => operate(async () => {
  resume(await request('discovery', { action: 'manual', setup_revision: saved.revision, discovery_revision: discoveryState.revision }));
}));
element('discovery-form').addEventListener('submit', (event) => {
  event.preventDefault();
  operate(async () => {
    const body = { action: discoveryAction, setup_revision: saved.revision, discovery_revision: discoveryState.revision,
      username: element('navidrome-admin').value, password: element('navidrome-password').value,
      recreate: discoveryAction === 'publish' && !!discoveryState.canRecreate,
      ...(pendingListeners ? { listeners: pendingListeners } : {}) };
    // Clear DOM values as soon as the request begins, including failed requests.
    element('discovery-form').reset();
    try { resume(await request('discovery', body)); }
    catch (error) {
      // The server may have saved an interrupted/failed operation. Reload its
      // public state so Retry can reconcile it with fresh credentials.
      if (error.status !== 401) {
        try {
          const value = await request('setup');
          if (body.listeners) {
            // Keep the draft and sign-in form after a failure. Refresh only the
            // discovery revision; never silently rebase over another settings edit.
            discoveryState = value.discovery;
            renderDiscovery();
            if (value.setup.revision !== saved.revision) element('reload').hidden = false;
            if (discoveryState.canRecreate) element('discovery-submit').textContent = 'Recreate and save';
          } else resume(value);
        } catch { /* Keep the original error. */ }
      }
      throw error;
    } finally { body.password = ''; body.username = ''; }
  });
});
function storageBytes(value) {
  const unit = value >= 1e9 ? 'GB' : 'MB';
  return `${(value / (unit === 'GB' ? 1e9 : 1e6)).toLocaleString(undefined, { maximumFractionDigits: 2 })} ${unit}`;
}
function usageText(id, used, limit) {
  const node = element(id); node.replaceChildren(document.createTextNode(storageBytes(used)));
  const capacity = document.createElement('span'); capacity.className = 'capacity'; capacity.textContent = ` / ${storageBytes(limit)}`; node.append(capacity);
}
function usageMeter(id, used, limit, reserved = 0) {
  const meter = element(id);
  const percent = Math.min(100, Math.max(0, limit > 0 ? used / limit * 100 : 0));
  meter.querySelector('.meter-fill').style.width = `${percent}%`;
  const reserve = meter.querySelector('.meter-reserved');
  if (reserve) reserve.style.width = `${Math.min(100, Math.max(0, limit > 0 ? reserved / limit * 100 : 0))}%`;
  meter.setAttribute('aria-valuenow', String(percent));
  meter.setAttribute('aria-valuetext', `${storageBytes(used)} used of ${storageBytes(limit)}${reserve ? `; ${storageBytes(reserved)} reserved` : ''}`);
  meter.dataset.over = String(used > limit);
}
function cancelStorage() {
  pendingStorage = null;
  element('cache-confirmation').hidden = true;
}
function renderStorage() {
  element('storage-form').hidden = !storageState;
  element('resources-form').hidden = !storageState?.resources;
  if (!storageState) return;
  renderStorageUsage(storageState);
  const { settings } = storageState;
  element('cache-enabled').value = String(settings.enabled);
  element('cache-limit').value = String(settings.limit_bytes / 1e9);
  if (storageState.resources) {
    const { resources } = storageState;
    element('resource-conversions').value = resources.max_conversions === null ? 'automatic' : String(resources.max_conversions);
    element('resource-working').value = String(resources.working_bytes / 1e6);
  }
}
function renderStorageUsage(state) {
  const { settings, cache, working_bytes: working } = state;
  for (const id of ['storage-usage', 'overview-cache-usage']) usageText(id, cache.retained_bytes, settings.limit_bytes);
  for (const id of ['cache-meter', 'overview-cache-meter']) usageMeter(id, cache.retained_bytes, settings.limit_bytes);
  element('overview-cache-state').textContent = settings.enabled ? 'On' : 'Off';
  const problem = cache.problem ? 'Cache storage is unavailable. Sidecar will retry.' : '';
  element('storage-problem').textContent = problem;
  element('overview-storage-problem').textContent = problem;
  element('overview-storage-problem').hidden = !problem;
  if (state.resources) {
    const { resources, runtime } = state;
    const jobs = runtime.conversions;
    element('resources-status').textContent = `${jobs.running} running · ${jobs.waiting} waiting · limit ${jobs.limit}${resources.max_conversions === null ? ' (Automatic)' : ''}`;
    for (const id of ['storage-working', 'overview-working-usage']) usageText(id, working, resources.working_bytes);
    for (const id of ['working-meter', 'overview-working-meter']) usageMeter(id, working, resources.working_bytes, runtime.working.reserved_bytes);
    for (const id of ['storage-reserved', 'overview-reserved']) element(id).textContent = `${storageBytes(runtime.working.reserved_bytes)} reserved · temporary`;
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
  // Refresh readings only: drafts and their original save revisions stay intact.
  renderStorageUsage(await request('storage'));
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
      element('message').textContent = 'Resource settings saved.';
    } finally { renderStorage(); }
  });
});
function stopDashboard() {
  dashboardEpoch++;
  clearTimeout(dashboardTimer);
  dashboardAbort?.abort();
  dashboardAbort = null;
}
function renderDashboard(value) {
  const clients = value.activity.clients;
  element('live-clients').textContent = String(clients.length);
  element('live-streams').textContent = String(clients.reduce((n, client) => n + client.streams.length, 0));
  const rate = value.bytes_per_second;
  element('live-rate').textContent = rate >= 1e6 ? `${(rate / 1e6).toFixed(2)} MB/s`
    : rate >= 1000 ? `${(rate / 1000).toFixed(1)} kB/s` : `${rate} B/s`;
  const jobs = value.storage.runtime.conversions;
  element('live-jobs').textContent = String(jobs.running);
  element('live-waiting').textContent = jobs.waiting ? `${jobs.waiting} waiting` : 'None waiting';
  element('live-health').textContent = value.ready ? 'Running' : 'Setup incomplete';
  element('live-health').dataset.state = value.ready ? 'good' : 'problem';
  const health = value.navidrome;
  element('live-navidrome').textContent = health.status === 'responding' ? 'Online'
    : health.status === 'problem' ? 'Connection problem' : health.status === 'checking' ? 'Checking…' : 'Not configured';
  element('live-navidrome').dataset.state = health.status === 'responding' ? 'good' : health.status === 'problem' ? 'problem' : 'checking';
  element('live-checked').textContent = health.checked_seconds_ago === undefined ? '' : `Checked ${health.checked_seconds_ago}s ago`;
  element('live-connection-error').hidden = health.status !== 'problem';
  element('live-connection-error').textContent = health.status === 'problem' ? `${health.message || 'Couldn’t reach Navidrome.'}${health.http_status ? ` (HTTP ${health.http_status})` : ''}` : '';
  const list = element('live-list');
  list.replaceChildren();
  if (!clients.length) {
    const item = document.createElement('li'); item.className = 'empty-state'; item.textContent = 'No listener sessions.'; list.append(item);
  }
  const qualities = { lossless: 'Original (lossless)', 'opus-64': 'Opus 64 kbps', 'opus-128': 'Opus 128 kbps',
    'opus-192': 'Opus 192 kbps', 'opus-256': 'Opus 256 kbps', 'opus-320': 'Opus 320 kbps' };
  for (const client of clients) {
    const item = document.createElement('li');
    const name = document.createElement('strong'); name.textContent = `Session ${client.id}`; item.append(name);
    for (const stream of client.streams) {
      const line = document.createElement('p'); line.className = 'stream-line';
      const streamName = document.createElement('span'); streamName.textContent = `Stream ${stream.id}`;
      const quality = document.createElement('span'); quality.className = 'stream-quality';
      quality.textContent = qualities[stream.quality] || 'Prepared; no audio sent yet';
      if (stream.quality) {
        const sent = document.createElement('span'); sent.className = 'hint'; sent.textContent = `Sent ${stream.sent_seconds_ago}s ago`; quality.append(sent);
      }
      line.append(streamName, quality); item.append(line);
    }
    if (!client.streams.length) {
      const line = document.createElement('p'); line.className = 'stream-line'; line.textContent = 'No open stream'; item.append(line);
    }
    list.append(item);
  }
  renderStorageUsage(value.storage);
  element('dashboard-status').textContent = 'Live · every 2s';
  element('dashboard-status').dataset.state = 'live';
}
function startDashboard() {
  stopDashboard();
  if (!dashboardWanted || document.hidden) return;
  const epoch = dashboardEpoch;
  element('dashboard-status').textContent = element('live-clients').textContent === '—'
    ? 'Loading…' : 'Refreshing…';
  element('dashboard-status').dataset.state = 'stale';
  const poll = async () => {
    if (epoch !== dashboardEpoch || !dashboardWanted || document.hidden) return;
    if (!busy) {
      const controller = new AbortController();
      dashboardAbort = controller;
      const timeout = setTimeout(() => controller.abort(), 8000);
      try {
        const value = await request('dashboard', undefined, controller.signal);
        if (epoch === dashboardEpoch) renderDashboard(value);
      } catch (error) {
        if (epoch !== dashboardEpoch) return;
        element('dashboard-status').textContent = 'Out of date · retrying…';
        element('dashboard-status').dataset.state = 'stale';
        if (error.status === 401 && !busy) {
          dashboardWanted = false; stopDashboard();
          await operate(refresh);
          return;
        }
      } finally {
        clearTimeout(timeout);
        if (dashboardAbort === controller) dashboardAbort = null;
      }
    }
    if (epoch === dashboardEpoch) dashboardTimer = setTimeout(poll, 2000);
  };
  poll();
}
document.addEventListener('visibilitychange', () => {
  if (document.hidden) {
    stopDashboard();
    element('dashboard-status').textContent = 'Updates paused';
  } else if (dashboardWanted) startDashboard();
});
window.addEventListener('pagehide', stopDashboard);
window.addEventListener('pageshow', () => { if (dashboardWanted) startDashboard(); });
operate(refresh);
