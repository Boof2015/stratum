'use strict';
const element = (id) => document.getElementById(id);
let needsOwner = false;
let submitting = false;

async function request(path, body) {
  const response = await fetch(`/admin/api/${path}`, body === undefined ? {} : {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  if (!response.ok) {
    const error = new Error(value.error || 'Sidecar could not complete the request. Try again.');
    error.status = response.status;
    throw error;
  }
  return value;
}

async function refresh() {
  element('retry').hidden = true;
  element('owner-form').hidden = true;
  element('ready').hidden = true;
  try {
    const status = await request('status');
    needsOwner = !status.ownerConfigured;
    if (!needsOwner) {
      try {
        await request('setup');
        element('title').textContent = 'Owner access is ready';
        element('description').textContent = 'You’re signed in to Sidecar.';
        element('ready').hidden = false;
        return;
      } catch (error) {
        if (error.status !== 401) throw error;
      }
    }
    element('title').textContent = needsOwner ? 'Make Sidecar yours' : 'Welcome back';
    element('description').textContent = needsOwner
      ? 'Enter the setup code from your server’s terminal or container logs, then choose a password of at least 12 characters.'
      : 'Sign in with your Sidecar administration password.';
    element('code-label').hidden = !needsOwner;
    element('code').required = needsOwner;
    element('confirm-label').hidden = !needsOwner;
    element('confirm').required = needsOwner;
    element('password').autocomplete = needsOwner ? 'new-password' : 'current-password';
    element('submit').textContent = needsOwner ? 'Create owner password' : 'Sign in';
    element('owner-form').hidden = false;
  } catch (error) {
    element('title').textContent = 'Couldn’t open Sidecar';
    element('message').textContent = error.message;
    element('retry').hidden = false;
  }
}

element('owner-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  if (submitting) return;
  element('message').textContent = '';
  const password = element('password').value;
  if (needsOwner && password !== element('confirm').value) {
    element('message').textContent = 'The passwords don’t match.';
    return;
  }
  submitting = true;
  element('submit').disabled = true;
  try {
    await request(needsOwner ? 'claim' : 'login', needsOwner
      ? { password, code: element('code').value.trim() }
      : { password });
    element('owner-form').reset();
    await refresh();
  } catch (error) {
    element('password').value = '';
    element('confirm').value = '';
    element('message').textContent = error.message;
    // A different browser/terminal might have completed enrollment meanwhile.
    if (error.status === 401) await refresh();
  } finally {
    submitting = false;
    element('submit').disabled = false;
  }
});
element('logout').addEventListener('click', async () => {
  element('message').textContent = '';
  try { await request('logout', {}); await refresh(); }
  catch (error) { element('message').textContent = error.message; }
});
element('retry').addEventListener('click', () => {
  element('message').textContent = '';
  refresh();
});
refresh();
