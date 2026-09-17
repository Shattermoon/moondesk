const $ = (id) => document.getElementById(id);
let currentContext = null;
let workspaceList = [];
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

function showError(error) {
  $('error').textContent = error ? String(error.message || error) : '';
}

function profileText(profile) {
  if (!profile) return 'Worker profile unavailable.';
  return `Current worker profile: ${profile.modelLabel} / ${CONFIG_EFFORT_LABEL[profile.reasoningEffort] || profile.reasoningEffort}`;
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
  $('baseUrl').value = status.baseUrl || $('baseUrl').value;
  const connected = Boolean(status.paired && status.connected !== false);
  $('pairing').hidden = connected;
  $('binding').hidden = !connected;
  $('profile').hidden = !connected;
  $('status').textContent = !status.paired
    ? 'Not paired'
    : status.connected === false
      ? `Paired, MoonDesk offline: ${status.error || 'connection failed'}`
      : 'Paired and connected';

  $('blocked').hidden = !status.blockedCommand;
  if (status.blockedCommand) {
    const retryMode = status.blockedCommand.retryMode || 'reconcile';
    $('blockedText').textContent = `Worker launch ${status.blockedCommand.commandId} is paused: ${status.blockedCommand.reason}`;
    $('retry').disabled = retryMode === 'none';
    $('retry').textContent = retryMode === 'fresh'
      ? 'Retry launch'
      : retryMode === 'none'
        ? 'Manual inspection required'
        : 'Retry reconciliation';
  } else {
    $('retry').disabled = false;
    $('retry').textContent = 'Retry reconciliation';
  }

  if (!connected) return;
  workspaceList = await bg({ type: 'MOONDESK_WORKSPACES' });
  currentProfile = await bg({ type: 'MOONDESK_PROFILE' });
  $('currentProfile').textContent = profileText(currentProfile);

  const select = $('workspace');
  select.textContent = '';
  for (const workspace of workspaceList) {
    const option = document.createElement('option');
    option.value = workspace.workspaceId;
    option.textContent = `${workspace.name} — ${workspace.root}`;
    select.append(option);
  }

  currentContext = await currentChatContext();
  $('projectContext').textContent = currentContext?.projectId && currentContext?.conversationId
    ? `Project ${currentContext.projectId} · conversation ${currentContext.conversationId}`
    : 'Open an existing conversation inside the ChatGPT Project you want to bind.';
  $('bind').disabled = !(currentContext?.projectId && currentContext?.conversationId && workspaceList.length);

  const bindings = await bg({ type: 'MOONDESK_BINDINGS' });
  const summaries = workspaceList
    .filter((workspace) => bindings[workspace.workspaceId])
    .map((workspace) => `${workspace.name} → ${bindings[workspace.workspaceId].projectId}`);
  $('bindings').textContent = summaries.length ? `Bindings: ${summaries.join(' · ')}` : 'No Project bindings yet.';

  if (modelCatalog.length) renderCatalog();
}

$('pair').addEventListener('click', async () => {
  showError('');
  try {
    await bg({
      type: 'MOONDESK_PAIR',
      baseUrl: $('baseUrl').value.trim(),
      pairingToken: $('pairingToken').value.trim()
    });
    $('pairingToken').value = '';
    await render();
  } catch (error) {
    showError(error);
  }
});

$('bind').addEventListener('click', async () => {
  showError('');
  try {
    if (!currentContext) throw new Error('ChatGPT Project context is unavailable');
    await bg({
      type: 'MOONDESK_BIND_PROJECT',
      workspaceId: $('workspace').value,
      context: currentContext
    });
    await render();
  } catch (error) {
    showError(error);
  }
});

$('discoverModels').addEventListener('click', async () => {
  showError('');
  const button = $('discoverModels');
  button.disabled = true;
  $('catalogStatus').textContent = 'Inspecting ChatGPT model picker…';
  try {
    modelCatalog = await currentModelCatalog();
    renderCatalog();
  } catch (error) {
    modelCatalog = [];
    $('catalogStatus').textContent = '';
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
