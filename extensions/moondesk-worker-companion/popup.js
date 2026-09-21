const $ = (id) => document.getElementById(id);
const MODEL_CATALOG_STORAGE_KEY = 'moondeskWorkerModelCatalogV1';
let currentContext = null;
let modelCatalog = [];
let currentProfile = null;

const PROVIDER_TO_CONFIG_EFFORT = {
  none: 'instant',
  low: 'low',
  medium: 'medium',
  high: 'high',
  xhigh: 'extra_high'
};
const CONFIG_EFFORT_LABEL = {
  instant: 'Instant',
  low: 'Low',
  medium: 'Medium',
  high: 'High',
  extra_high: 'Extra High'
};

async function bg(message) {
  const response = await chrome.runtime.sendMessage(message);
  if (!response?.ok) throw new Error(response?.error || 'MoonDesk companion request failed');
  return response.result;
}

async function activeChatTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab?.id && tab.url?.startsWith('https://chatgpt.com/') ? tab : null;
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

async function currentModelCatalog() {
  const tab = await activeChatTab();
  if (!tab) throw new Error('Open ChatGPT before discovering models');
  await ensureChatScripts(tab.id);
  const response = await chrome.tabs.sendMessage(tab.id, { type: 'MOONDESK_MODEL_CATALOG' });
  if (!response?.ok || !Array.isArray(response.catalog)) {
    throw new Error(response?.error || 'Could not confirm ChatGPT model catalog');
  }
  return response.catalog;
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

function showError(error) {
  $('error').textContent = error ? String(error.message || error) : '';
}

function profileText(profile) {
  if (!profile) return 'Worker profile unavailable.';
  return `Current worker profile: ${profile.modelLabel} / ${CONFIG_EFFORT_LABEL[profile.reasoningEffort] || profile.reasoningEffort}`;
}

function renderBrowserClients(clients) {
  const list = $('browserList');
  list.textContent = '';
  const stale = (clients || []).filter((client) => !client.current);
  $('browserManager').hidden = stale.length === 0;
  for (const client of clients || []) {
    const row = document.createElement('div');
    row.className = 'browser-row';
    const text = document.createElement('div');
    text.className = 'browser-copy';
    const label = client.browserLabel || 'Browser';
    const presence = client.online ? 'online' : 'offline';
    text.textContent = `${label} · ${presence} · ${client.clientId}${client.current ? ' · current' : ''}`;
    row.append(text);
    if (!client.current) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'browser-revoke';
      button.dataset.clientId = client.clientId;
      button.textContent = 'Revoke';
      row.append(button);
    }
    list.append(row);
  }
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
    const matching = [...select.options].find((option) => option.value === currentProfile.reasoningEffort);
    if (matching) select.value = matching.value;
  }
  $('effortLabel').hidden = efforts.length === 0;
  $('saveProfile').hidden = efforts.length === 0;
}

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
    $('catalogStatus').textContent = 'No MoonDesk-supported model/effort combinations were confirmed.';
    return;
  }
  const configured = usable.find((model) => profileBelongsToModel(currentProfile, model));
  if (configured) select.value = configured.id;
  $('catalogStatus').textContent = `${usable.length} available model famil${usable.length === 1 ? 'y' : 'ies'} confirmed from this ChatGPT account.`;
  renderEfforts();
}

async function render() {
  showError('');
  const status = await bg({ type: 'MOONDESK_STATUS' });
  const connected = status.connected === true;
  $('routing').hidden = !connected;
  $('profile').hidden = !connected;
  $('manualRepair').hidden = !status.repairRequired;
  $('manualRepair').open = Boolean(status.repairRequired);
  $('status').textContent = connected
    ? 'Connected'
    : status.repairRequired
      ? 'Manual repair required'
      : status.errorCode === 'bridge_not_found'
        ? 'MoonDesk companion bridge not found'
        : 'Connecting to MoonDesk…';
  $('bridgeStatus').textContent = status.baseUrl
    ? `Local bridge: ${status.baseUrl}`
    : 'Searching for local MoonDesk…';
  if (!connected && status.error) showError(status.error);

  const blockedCommands = Array.isArray(status.blockedCommands)
    ? status.blockedCommands
    : status.blockedCommand
      ? [status.blockedCommand]
      : [];
  const retryableBlocked = blockedCommands.filter((entry) => entry.retryMode !== 'none');
  const manualBlocked = blockedCommands.length - retryableBlocked.length;
  $('blocked').hidden = blockedCommands.length === 0;
  if (blockedCommands.length) {
    const first = blockedCommands[0];
    $('blockedText').textContent = blockedCommands.length === 1
      ? `Worker launch ${first.commandId} is paused: ${first.reason}`
      : `${blockedCommands.length} worker launches need attention (${retryableBlocked.length} retryable, ${manualBlocked} manual inspection).`;
    $('retry').disabled = retryableBlocked.length === 0;
    $('retry').textContent = retryableBlocked.length > 1
      ? `Retry ${retryableBlocked.length} safe launches`
      : retryableBlocked.length === 1
        ? 'Retry safe launch'
        : 'Manual inspection required';
  } else {
    $('retry').disabled = false;
    $('retry').textContent = 'Retry reconciliation';
  }

  if (!connected) return;
  currentProfile = await bg({ type: 'MOONDESK_PROFILE' });
  const pairedClients = await bg({ type: 'MOONDESK_CLIENTS' });
  renderBrowserClients(pairedClients);
  modelCatalog = await loadCachedModelCatalog();
  $('currentProfile').textContent = profileText(currentProfile);
  $('browserStatus').textContent = `${status.pairedClientCount || 1} paired browser installation${(status.pairedClientCount || 1) === 1 ? '' : 's'} · this client ${status.clientId || 'connected'}`;

  currentContext = await currentChatContext();
  $('chatContext').textContent = currentContext?.conversationId
    ? currentContext.projectId
      ? `Current chat: Project ${currentContext.projectId} · conversation ${currentContext.conversationId}`
      : `Current chat: normal conversation ${currentContext.conversationId}`
    : 'Open an existing ChatGPT conversation to make it an Anchor for worker routing.';

  if (modelCatalog.length) renderCatalog();
}

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

$('browserList').addEventListener('click', async (event) => {
  const button = event.target.closest('button[data-client-id]');
  if (!button) return;
  const clientId = button.dataset.clientId;
  if (!clientId) return;
  if (!confirm(`Revoke paired browser ${clientId}? Any ambiguous launch pinned to it will be paused, not resent.`)) return;
  showError('');
  button.disabled = true;
  try {
    const result = await bg({ type: 'MOONDESK_REVOKE_CLIENT', clientId });
    $('browserStatus').textContent = `Stale browser revoked · ${result.retargetedCommands || 0} safe command(s) retargeted · ${result.pausedCommands || 0} ambiguous command(s) paused`;
    await render();
  } catch (error) {
    showError(error);
    button.disabled = false;
  }
});

$('discoverModels').addEventListener('click', async () => {
  showError('');
  const button = $('discoverModels');
  button.disabled = true;
  $('catalogStatus').textContent = 'Inspecting ChatGPT model picker…';
  try {
    modelCatalog = await currentModelCatalog();
    await saveCachedModelCatalog(modelCatalog);
    renderCatalog();
  } catch (error) {
    if (modelCatalog.length) {
      renderCatalog();
      $('catalogStatus').textContent = 'Using the last confirmed model catalog; refresh failed.';
    } else {
      $('catalogStatus').textContent = '';
    }
    showError(error);
  } finally {
    button.disabled = false;
  }
});

$('model').addEventListener('change', renderEfforts);

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
    $('currentProfile').textContent = profileText(currentProfile);
    $('catalogStatus').textContent = 'Worker profile saved to MoonDesk.';
  } catch (error) {
    showError(error);
  }
});

$('retry').addEventListener('click', async () => {
  showError('');
  try {
    await bg({ type: 'MOONDESK_RETRY_BLOCKED' });
    await render();
  } catch (error) {
    showError(error);
  }
});

void render().catch(showError);
