/* ── MoonDesk Worker Companion — popup.js ──────────────────────── */
/* eslint-env browser */
/* global chrome */

const $ = (id) => document.getElementById(id);
const MODEL_CATALOG_STORAGE_KEY = 'moondeskWorkerModelCatalogV1';
let currentContext = null;
let modelCatalog = [];
let currentProfile = null;

const DEFAULT_WORKER_MODEL_ALIAS = 'gpt-5.6-sol';
const DEFAULT_WORKER_EFFORT = 'high';

const PROVIDER_TO_CONFIG_EFFORT = {
  none: 'instant',
  minimal: 'minimal',
  low: 'low',
  medium: 'medium',
  high: 'high',
  xhigh: 'extra_high',
  max: 'max',
  ultra: 'ultra',
  pro: 'pro'
};
const CONFIG_EFFORT_LABEL = {
  instant: 'Instant',
  minimal: 'Minimal',
  low: 'Low',
  medium: 'Medium',
  high: 'High',
  extra_high: 'Extra High',
  max: 'Max',
  ultra: 'Ultra',
  pro: 'Pro'
};

// ── Background messaging ─────────────────────────────────────────
async function bg(message) {
  const response = await chrome.runtime.sendMessage(message);
  if (!response?.ok) throw new Error(response?.error || 'MoonDesk companion request failed');
  return response.result;
}

// ── ChatGPT context detection ────────────────────────────────────
async function activeChatTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return Number.isInteger(tab?.id) && tab.url?.startsWith('https://chatgpt.com/') ? tab : null;
}

async function ensureChatScripts(tabId) {
  try {
    await chrome.scripting.executeScript({
      target: { tabId },
      world: 'MAIN',
      files: ['model-state-main.js']
    });
  } catch {}
  await chrome.scripting.executeScript({
    target: { tabId },
    files: ['chatgpt-dom.js', 'content.js']
  });
}

async function currentChatContext() {
  const tab = await activeChatTab();
  if (!tab) return null;
  try {
    await ensureChatScripts(tab.id);
    const response = await chrome.tabs.sendMessage(tab.id, { type: 'MOONDESK_CONTEXT' });
    return response?.ok ? response.context : null;
  } catch {
    return null;
  }
}

// ── Model catalog ────────────────────────────────────────────────
async function currentModelCatalog() {
  const catalog = await bg({ type: 'MOONDESK_DISCOVER_MODELS' });
  if (!Array.isArray(catalog) || !catalog.length) {
    throw new Error('Could not confirm ChatGPT model catalog');
  }
  return catalog;
}

async function loadCachedModelCatalog() {
  try {
    const stored = await chrome.storage.local.get(MODEL_CATALOG_STORAGE_KEY);
    const cached = stored[MODEL_CATALOG_STORAGE_KEY];
    return Array.isArray(cached?.catalog) ? cached.catalog : [];
  } catch {
    return [];
  }
}

async function saveCachedModelCatalog(catalog) {
  await chrome.storage.local.set({
    [MODEL_CATALOG_STORAGE_KEY]: {
      catalog,
      updatedAt: Date.now()
    }
  });
}

// ── UI helpers ───────────────────────────────────────────────────
function showError(error) {
  $('error').textContent = error ? String(error.message || error) : '';
}

function profileText(profile) {
  if (!profile) return 'Not configured';
  const effortLabel = CONFIG_EFFORT_LABEL[profile.reasoningEffort] || profile.reasoningEffort;
  return `${profile.modelLabel} · ${effortLabel}`;
}

function supportedEfforts(model) {
  const unique = new Map();
  for (const choice of model?.choices || []) {
    const config = PROVIDER_TO_CONFIG_EFFORT[choice.effort];
    if (!config || !choice.id) continue;
    if (!unique.has(config)) unique.set(config, { provider: choice.effort, config, modelKey: choice.id });
  }
  return [...unique.values()];
}

function profileBelongsToModel(profile, model) {
  return Boolean(
    profile &&
    model &&
    (profile.modelKey === model.id || (model.choices || []).some((choice) => choice.id === profile.modelKey))
  );
}

function isPreferredDefaultModel(model) {
  return Boolean(
    model &&
    (model.id === DEFAULT_WORKER_MODEL_ALIAS ||
      (model.choices || []).some((choice) => choice.id === DEFAULT_WORKER_MODEL_ALIAS)) &&
    supportedEfforts(model).some((effort) => effort.config === DEFAULT_WORKER_EFFORT)
  );
}

// ── Render: effort selector ──────────────────────────────────────
function renderEfforts() {
  const selectedModel = modelCatalog.find((entry) => entry.id === $('model').value);
  const efforts = supportedEfforts(selectedModel);
  const select = $('effort');
  select.textContent = '';
  for (const effort of efforts) {
    const option = document.createElement('option');
    option.value = effort.config;
    option.textContent = CONFIG_EFFORT_LABEL[effort.config] || effort.config;
    select.append(option);
  }
  if (profileBelongsToModel(currentProfile, selectedModel)) {
    let matching = [...select.options].find((option) => option.value === currentProfile.reasoningEffort);
    // Legacy saved Extra High profiles remain usable when the current picker exposes the same
    // lane as Max. Prefer the exact legacy xhigh option whenever the provider still offers it.
    if (!matching && currentProfile.reasoningEffort === 'extra_high') {
      matching = [...select.options].find((option) => option.value === 'max');
    }
    if (matching) select.value = matching.value;
  } else if (isPreferredDefaultModel(selectedModel)) {
    const preferred = [...select.options].find((option) => option.value === DEFAULT_WORKER_EFFORT);
    if (preferred) select.value = preferred.value;
  }
  $('effortLabel').hidden = efforts.length === 0;
  $('saveProfile').hidden = efforts.length === 0;
}

// ── Render: model catalog ────────────────────────────────────────
function renderCatalog() {
  const usable = modelCatalog.filter((model) => supportedEfforts(model).length > 0);
  const select = $('model');
  select.textContent = '';
  for (const model of usable) {
    const option = document.createElement('option');
    option.value = model.id;
    option.textContent = model.label;
    select.append(option);
  }
  $('modelLabel').hidden = usable.length === 0;
  $('effortLabel').hidden = usable.length === 0;
  $('saveProfile').hidden = usable.length === 0;
  if (!usable.length) {
    $('catalogStatus').textContent = 'No supported models confirmed.';
    return;
  }
  const configured = usable.find((model) => profileBelongsToModel(currentProfile, model));
  const preferredDefault = usable.find(isPreferredDefaultModel);
  const selected = configured || preferredDefault || usable[0];
  if (selected) select.value = selected.id;
  $('catalogStatus').textContent = `${usable.length} model${usable.length === 1 ? '' : 's'} available`;
  renderEfforts();
}

// ── Render: paired browsers ──────────────────────────────────────
function renderBrowserClients(clients) {
  const list = $('browserList');
  list.textContent = '';
  const all = clients || [];
  const stale = all.filter((client) => !client.current);
  $('browsersBlock').hidden = all.length === 0;
  if (!all.length) return;

  for (const client of all) {
    const row = document.createElement('div');
    row.className = 'browser-row';

    const dot = document.createElement('span');
    dot.className = `browser-dot ${client.online ? 'browser-dot-online' : 'browser-dot-offline'}`;
    dot.setAttribute('aria-label', client.online ? 'Online' : 'Offline');
    row.append(dot);

    const text = document.createElement('span');
    text.className = `browser-copy${client.current ? ' browser-current' : ''}`;
    const label = client.browserLabel || 'Browser';
    text.textContent = client.current ? `${label} (this)` : label;
    text.title = client.clientId;
    row.append(text);

    if (!client.current) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'btn btn-subtle btn-sm browser-revoke';
      button.dataset.clientId = client.clientId;
      button.textContent = 'Revoke';
      button.setAttribute('aria-label', `Revoke paired browser ${client.clientId}`);
      row.append(button);
    }
    list.append(row);
  }

  $('browserStatus').textContent = `${all.length} paired browser${all.length === 1 ? '' : 's'}${stale.length ? ` · ${stale.length} stale` : ''}`;
}

// ── Render: clear workers button state ───────────────────────────
function renderClearWorkers() {
  const hasConversation = Boolean(currentContext?.conversationId);
  $('clearBlock').hidden = false;
  $('clearWorkers').disabled = !hasConversation;
  if (!hasConversation) {
    $('clearWorkers').title = 'Open the Core conversation first';
  } else {
    $('clearWorkers').title = '';
  }
}

// ── Main render ──────────────────────────────────────────────────
async function render() {
  showError('');
  const status = await bg({ type: 'MOONDESK_STATUS' });
  const connected = status.connected === true;

  // Connection pill
  const pill = $('connPill');
  pill.className = 'conn-pill';
  if (connected) {
    pill.textContent = 'Connected';
    pill.classList.add('pill-connected');
  } else if (status.repairRequired) {
    pill.textContent = 'Repair needed';
    pill.classList.add('pill-repair');
  } else if (status.errorCode === 'bridge_not_found') {
    pill.textContent = 'Bridge not found';
    pill.classList.add('pill-error');
  } else {
    pill.textContent = 'Connecting…';
    pill.classList.add('pill-checking');
  }

  // Cards visibility
  $('coreCard').hidden = !connected;
  $('profileCard').hidden = !connected;
  $('advancedSection').hidden = !connected;

  // Bridge status
  $('bridgeValue').textContent = status.baseUrl
    ? new URL(status.baseUrl).host
    : '—';

  // Repair block visibility
  $('repairBlock').hidden = !status.repairRequired;

  if (!connected && status.error) showError(status.error);
  if (!connected) return;

  // Profile
  currentProfile = await bg({ type: 'MOONDESK_PROFILE' });
  const profileEl = $('currentProfile');
  const text = profileText(currentProfile);
  profileEl.textContent = text;
  if (currentProfile) {
    profileEl.classList.add('profile-active');
    profileEl.classList.remove('dim');
  } else {
    profileEl.classList.remove('profile-active');
    profileEl.classList.add('dim');
  }

  // Clients
  const pairedClients = await bg({ type: 'MOONDESK_CLIENTS' });
  renderBrowserClients(pairedClients);

  // Cached model catalog
  modelCatalog = await loadCachedModelCatalog();
  if (modelCatalog.length) {
    renderCatalog();
  } else {
    $('catalogStatus').textContent = 'Models are discovered automatically when ChatGPT is available.';
  }

  // Chat context detection
  currentContext = await currentChatContext();
  const chatEl = $('coreChat');
  if (currentContext?.conversationId) {
    chatEl.innerHTML = '';
    const convSpan = document.createElement('span');
    convSpan.className = 'core-conv';
    convSpan.textContent = currentContext.conversationId;
    chatEl.append(convSpan);
    if (currentContext.projectId) {
      const projSpan = document.createElement('span');
      projSpan.className = 'core-proj';
      projSpan.textContent = ` · Project`;
      projSpan.title = currentContext.projectId;
      chatEl.append(projSpan);
    }
    chatEl.classList.remove('dim');
  } else {
    chatEl.textContent = 'Open a ChatGPT conversation';
    chatEl.classList.add('dim');
  }

  // Advanced: clear workers
  renderClearWorkers();
}

// ── Event: repair pairing ────────────────────────────────────────
$('pair').addEventListener('click', async () => {
  showError('');
  try {
    await bg({
      type: 'MOONDESK_PAIR',
      pairingToken: $('pairingToken').value.trim()
    });
    $('pairingToken').value = '';
    await render();
  } catch (error) {
    showError(error);
  }
});

// ── Event: revoke paired browser ─────────────────────────────────
$('browserList').addEventListener('click', async (event) => {
  const button = event.target.closest('button[data-client-id]');
  if (!button) return;
  const clientId = button.dataset.clientId;
  if (!clientId) return;
  if (!confirm(`Revoke paired browser ${clientId}?\n\nAmbiguous launches will be paused, not resent.`)) return;
  showError('');
  button.disabled = true;
  try {
    await bg({ type: 'MOONDESK_REVOKE_CLIENT', clientId });
    await render();
  } catch (error) {
    showError(error);
    button.disabled = false;
  }
});

// ── Event: discover models ───────────────────────────────────────
$('discoverModels').addEventListener('click', async () => {
  showError('');
  const button = $('discoverModels');
  button.disabled = true;
  $('catalogStatus').textContent = 'Inspecting models…';
  try {
    modelCatalog = await currentModelCatalog();
    await saveCachedModelCatalog(modelCatalog);
    renderCatalog();
  } catch (error) {
    if (modelCatalog.length) {
      renderCatalog();
      $('catalogStatus').textContent = 'Using cached catalog; refresh failed.';
    } else {
      $('catalogStatus').textContent = '';
    }
    showError(error);
  } finally {
    button.disabled = false;
  }
});

// ── Event: model change ──────────────────────────────────────────
$('model').addEventListener('change', renderEfforts);

// ── Event: save profile ──────────────────────────────────────────
$('saveProfile').addEventListener('click', async () => {
  showError('');
  try {
    const model = modelCatalog.find((entry) => entry.id === $('model').value);
    const reasoningEffort = $('effort').value;
    const choice = supportedEfforts(model).find((entry) => entry.config === reasoningEffort);
    if (!model || !reasoningEffort || !choice?.modelKey) {
      throw new Error('Select a confirmed model and reasoning effort');
    }
    currentProfile = await bg({
      type: 'MOONDESK_SET_PROFILE',
      profile: {
        modelKey: choice.modelKey,
        modelLabel: model.label,
        reasoningEffort
      }
    });
    const profileEl = $('currentProfile');
    profileEl.textContent = profileText(currentProfile);
    profileEl.classList.add('profile-active');
    profileEl.classList.remove('dim');
    $('catalogStatus').textContent = 'Profile saved.';
  } catch (error) {
    showError(error);
  }
});

// ── Event: clear workers ─────────────────────────────────────────
$('clearWorkers').addEventListener('click', async () => {
  if (!currentContext?.conversationId) return;
  if (!confirm('Clear all worker history and bindings for this Core conversation?\n\nChatGPT chats are preserved. Active/ambiguous work will be refused.')) return;
  showError('');
  $('clearWorkers').disabled = true;
  try {
    const result = await bg({
      type: 'MOONDESK_CLEAR_WORKERS',
      conversationId: currentContext.conversationId
    });
    const count = (result.workerIds?.length || 0);
    $('catalogStatus').textContent = `Cleared ${count} worker${count === 1 ? '' : 's'}.`;
    await render();
  } catch (error) {
    showError(error);
    $('clearWorkers').disabled = false;
  }
});

// ── Automatic catalog updates ────────────────────────────────────
chrome.storage?.onChanged?.addListener?.((changes, areaName) => {
  if (areaName !== 'local') return;
  const cached = changes?.[MODEL_CATALOG_STORAGE_KEY]?.newValue;
  if (!Array.isArray(cached?.catalog) || !cached.catalog.length) return;
  modelCatalog = cached.catalog;
  renderCatalog();
});

// ── Boot ─────────────────────────────────────────────────────────
void render().catch(showError);
