'use strict';
const element = (id) => document.getElementById(id);
let needsOwner = false;
let busy = false;
let saved = null;

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
  if (step === 'summary') {
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
  try {
    const status = await request('status');
    needsOwner = !status.ownerConfigured;
    if (!needsOwner) {
      try { resume(await request('setup')); return; }
      catch (error) { if (error.status !== 401) throw error; }
    }
    saved = null;
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
operate(refresh);
