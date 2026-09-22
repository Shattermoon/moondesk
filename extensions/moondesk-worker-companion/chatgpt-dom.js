(() => {
  if (window.MOONDESK_CHATGPT_DOM) return;

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const visible = (node) => Boolean(
    node &&
    node.isConnected &&
    !node.closest('[hidden],[aria-hidden="true"],[inert]') &&
    node.getClientRects().length > 0
  );
  const compact = (value) => String(value || '').replace(/\s+/g, ' ').trim();
  const normalizeModel = (value) => String(value || '').toLowerCase().replace(/[^a-z0-9.]/g, '');
  const PROVIDER_EFFORTS = new Set(['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra', 'pro']);

  const projectIdFromPath = (pathname = location.pathname) => {
    const match = /^\/g\/(g-p-[0-9a-f]{32})(?:-[^/]+)?(?:\/|$)/i.exec(pathname);
    return match?.[1]?.toLowerCase() || null;
  };

  const conversationIdFromPath = (pathname = location.pathname) => {
    const match = /\/c\/([0-9a-f-]{16,64})(?:\/|$)/i.exec(pathname);
    return match?.[1] || null;
  };

  const composer = () => document.querySelector(
    '#prompt-textarea,[data-testid="prompt-textarea"][contenteditable="true"],textarea[data-testid="prompt-textarea"]'
  );
  const composerForm = () => composer()?.closest('form') || null;

  const sendButton = () => {
    const root = composerForm() || document;
    const candidates = [
      root.querySelector('button[data-testid="send-button"]'),
      root.querySelector('button[data-testid="composer-submit-button"]'),
      root.querySelector('button[aria-label*="Send" i]')
    ].filter(visible);
    return candidates[0] || null;
  };

  const generating = () => Boolean(
    [...document.querySelectorAll('button[data-testid="stop-button"],button[aria-label*="Stop" i]')].find(visible)
  );

  async function waitFor(read, timeoutMs = 12000, intervalMs = 80) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      try {
        const value = await read();
        if (value) return value;
      } catch {}
      await sleep(intervalMs);
    }
    return null;
  }

  const projectHome = () => Boolean(projectIdFromPath() && /\/project\/?$/i.test(location.pathname));

  function projectHomeUrl() {
    const projectId = projectIdFromPath();
    if (!projectId) return null;
    const match = /^\/g\/([^/]+)/i.exec(location.pathname);
    if (!match) return null;
    return location.origin + '/g/' + match[1] + '/project';
  }

  function projectLink(projectId) {
    const links = [...document.querySelectorAll('header a[href], [role="banner"] a[href]')].filter(visible);
    const matched = links.filter((link) => {
      try {
        const url = new URL(link.href, location.href);
        return (
          url.origin === location.origin &&
          projectIdFromPath(url.pathname) === projectId &&
          /\/project\/?$/i.test(url.pathname)
        );
      } catch {
        return false;
      }
    });
    return matched.length === 1 ? matched[0] : null;
  }

  async function enterProject(projectId) {
    if (projectIdFromPath() !== projectId) return false;
    if (projectHome()) {
      return Boolean(await waitFor(() => visible(composer()) && !generating(), 15000));
    }
    const link = await waitFor(() => projectLink(projectId), 10000);
    if (!link) return false;
    link.click();
    return Boolean(await waitFor(
      () => projectIdFromPath() === projectId && projectHome() && visible(composer()) && !generating(),
      15000
    ));
  }

  function userMessages() {
    const selectors = '[data-message-author-role="user"],[data-testid^="conversation-turn-"] [data-message-author-role="user"]';
    return [...document.querySelectorAll(selectors)].filter(visible);
  }

  function assistantMessages() {
    const selectors = '[data-message-author-role="assistant"],[data-testid^="conversation-turn-"] [data-message-author-role="assistant"]';
    return [...document.querySelectorAll(selectors)].filter(visible);
  }

  const taskMarkerPresent = (marker) => userMessages().some(
    (node) => (node.innerText || node.textContent || '').includes(marker)
  );

  function modelPickerTrigger() {
    const root = composerForm();
    if (!root) return null;
    const candidates = [...root.querySelectorAll('button[aria-haspopup="menu"]')].filter(
      (node) =>
        visible(node) &&
        node.id !== 'composer-plus-btn' &&
        node.getAttribute('data-testid') !== 'composer-plus-btn'
    );
    return candidates.length === 1 ? candidates[0] : null;
  }

  const pickerRoot = () => document.querySelector('[data-testid="composer-intelligence-picker-content"]');

  function providerEffort(reasoningEffort) {
    return ({
      instant: 'none',
      low: 'low',
      medium: 'medium',
      high: 'high',
      extra_high: 'xhigh'
    })[reasoningEffort] || null;
  }

  function validPickerState(state) {
    const groupId = (value) =>
      typeof value === 'string' &&
      /^[a-zA-Z0-9._ -]{1,80}$/.test(value) &&
      value.trim() === value &&
      value.trim();
    if (!state || typeof state !== 'object') return null;

    if (state.selected) {
      const selected = state.selected;
      return (
        typeof selected.id === 'string' &&
        /^[a-zA-Z0-9._-]{1,80}$/.test(selected.id) &&
        PROVIDER_EFFORTS.has(selected.effort)
      ) ? { selected: { id: selected.id, effort: selected.effort } } : null;
    }

    if (
      typeof state.version !== 'string' ||
      !Number.isInteger(state.currentBucket) ||
      !Array.isArray(state.versions) ||
      state.versions.length < 1 ||
      state.versions.length > 20 ||
      !Array.isArray(state.choices) ||
      state.choices.length < 1 ||
      state.choices.length > 12
    ) {
      return null;
    }
    if (
      !state.versions.every((version) =>
        groupId(version.id) &&
        typeof version.label === 'string' &&
        version.label.trim().length > 0 &&
        version.label.length <= 80
      ) ||
      !state.choices.every((choice) =>
        Number.isInteger(choice.bucket) &&
        typeof choice.id === 'string' &&
        /^[a-zA-Z0-9._-]{1,80}$/.test(choice.id) &&
        typeof choice.label === 'string' &&
        choice.label.length > 0 &&
        choice.label.length <= 80 &&
        groupId(choice.familyId) &&
        typeof choice.familyLabel === 'string' &&
        choice.familyLabel.length > 0 &&
        choice.familyLabel.length <= 80 &&
        PROVIDER_EFFORTS.has(choice.effort) &&
        typeof choice.available === 'boolean'
      ) ||
      new Set(state.versions.map((version) => version.id)).size !== state.versions.length ||
      new Set(state.choices.map((choice) => choice.bucket)).size !== state.choices.length ||
      !state.versions.some((version) => version.id === state.version) ||
      !state.choices.some((choice) => choice.bucket === state.currentBucket)
    ) {
      return null;
    }
    return state;
  }

  function readPickerState() {
    return new Promise((resolve) => {
      const nonce = crypto.randomUUID();
      let done = false;
      const finish = (value) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        window.removeEventListener('message', receive);
        resolve(value);
      };
      const receive = (event) => {
        const data = event.data;
        if (
          event.source !== window ||
          event.origin !== location.origin ||
          data?.source !== 'moondesk-picker-reply' ||
          data.nonce !== nonce ||
          data.v !== 1
        ) {
          return;
        }
        finish(validPickerState(data.picker));
      };
      const timer = setTimeout(() => finish(null), 1500);
      window.addEventListener('message', receive);
      window.postMessage({ source: 'moondesk-picker-ask', nonce, v: 1 }, location.origin);
    });
  }

  function readCorrelationEvidence() {
    return new Promise((resolve) => {
      const nonce = crypto.randomUUID();
      let done = false;
      const finish = (value) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        window.removeEventListener('message', receive);
        resolve(value);
      };
      const receive = (event) => {
        const data = event.data;
        if (
          event.source !== window ||
          event.origin !== location.origin ||
          data?.source !== 'moondesk-correlation-reply' ||
          data.nonce !== nonce ||
          data.v !== 1
        ) {
          return;
        }
        const value = data.correlation;
        const routeConversation = conversationIdFromPath();
        if (
          !value ||
          !routeConversation ||
          value.conversationId !== routeConversation ||
          !Array.isArray(value.requestIds) ||
          value.requestIds.length < 1 ||
          value.requestIds.length > 32 ||
          value.requestIds.some(
            (requestId) => typeof requestId !== 'string' || !/^[a-z0-9_-]{1,100}$/i.test(requestId)
          )
        ) {
          finish(null);
          return;
        }
        finish({
          conversationId: routeConversation,
          requestIds: [...new Set(value.requestIds)]
        });
      };
      const timer = setTimeout(() => finish(null), 1200);
      window.addEventListener('message', receive);
      window.postMessage({ source: 'moondesk-correlation-ask', nonce, v: 1 }, location.origin);
    });
  }

  async function prepareChatModelSurface() {
    const radios = () => [...document.querySelectorAll('[role="radio"][data-tpp-toggle-value]')].filter(visible);
    const state = () => {
      const nodes = radios();
      const chat = nodes.filter((node) => node.getAttribute('data-tpp-toggle-value') === 'chatgpt');
      const work = nodes.filter((node) => node.getAttribute('data-tpp-toggle-value') === 'work');
      return chat.length === 1 && work.length === 1 ? { chat: chat[0], work: work[0] } : null;
    };

    const before = state();
    if (!before) return radios().length === 0;
    if (before.chat.getAttribute('aria-checked') === 'true') return true;
    if (
      before.work.getAttribute('aria-checked') !== 'true' ||
      before.chat.disabled ||
      before.chat.getAttribute('aria-disabled') === 'true'
    ) {
      return false;
    }
    before.chat.click();
    return Boolean(await waitFor(() => {
      const next = state();
      return next?.chat.getAttribute('aria-checked') === 'true' &&
        next.work.getAttribute('aria-checked') === 'false';
    }, 5000));
  }

  function modelPickerAccess() {
    const state = async (predicate = null, timeoutMs = 3500) => waitFor(async () => {
      const value = await readPickerState();
      if (!value || value.selected) return null;
      return !predicate || predicate(value) ? value : null;
    }, timeoutMs, 90);

    const key = (node, value) => {
      if (!node) return false;
      node.focus();
      node.dispatchEvent(new KeyboardEvent('keydown', {
        key: value,
        code: value,
        bubbles: true,
        cancelable: true
      }));
      return true;
    };

    return {
      state,
      async open() {
        const trigger = await waitFor(modelPickerTrigger, 15000);
        if (!trigger || !(await prepareChatModelSurface())) return null;
        if (!pickerRoot()) {
          key(trigger, 'Enter');
          if (!(await waitFor(pickerRoot, 3500))) return null;
        }
        return state();
      },
      close() {
        key(modelPickerTrigger(), 'Escape');
      },
      async version(versionId) {
        const before = await state();
        if (!before) return null;
        const versionRows = () => [...(pickerRoot()?.querySelectorAll('[role="menuitemradio"]') || [])].filter(visible);
        if (before.version === versionId && !versionRows().length) return before;
        const versionLabel = before.versions.find((version) => version.id === versionId)?.label;
        if (!versionLabel) return null;

        if (!versionRows().length) {
          const toggles = [...pickerRoot().querySelectorAll('[role="menuitem"][aria-expanded]')].filter(visible);
          if (toggles.length !== 1) return null;
          toggles[0].click();
        }
        const option = await waitFor(() => {
          const rows = versionRows().filter((node) => {
            if (node.getAttribute('aria-disabled') === 'true') return false;
            const exactLabel = compact(node.textContent) === versionLabel ||
              [...node.querySelectorAll('*')].some(
                (child) => child.children.length === 0 && compact(child.textContent) === versionLabel
              );
            return exactLabel;
          });
          return rows.length === 1 ? rows[0] : null;
        }, 3500);
        if (!key(option, 'Enter')) return null;
        return state((next) => next.version === versionId && !versionRows().length);
      },
      async bucket(bucket) {
        let current = await state();
        for (let count = 0; current && count < 12; count += 1) {
          if (current.currentBucket === bucket) return current;
          const from = current.choices.findIndex((choice) => choice.bucket === current.currentBucket);
          const to = current.choices.findIndex((choice) => choice.bucket === bucket);
          if (from < 0 || to < 0) return null;
          const expected = current.choices[from + (to > from ? 1 : -1)].bucket;
          const controls = [...pickerRoot().querySelectorAll('[role="menuitem"][aria-keyshortcuts]')].filter(
            (node) => visible(node) && (node.getAttribute('aria-keyshortcuts') || '').includes('ArrowRight')
          );
          if (controls.length !== 1 || !key(controls[0], to > from ? 'ArrowRight' : 'ArrowLeft')) {
            return null;
          }
          const version = current.version;
          current = await state(
            (next) => next.version === version && next.currentBucket === expected,
            3500
          );
        }
        return null;
      }
    };
  }

  function modelMatches(choice, profile) {
    const requested = [profile?.modelKey, profile?.modelLabel].filter(
      (value) => typeof value === 'string' && value.trim()
    );
    return requested.some((model) =>
      choice.familyId === model ||
      choice.id === model ||
      normalizeModel(choice.familyLabel) === normalizeModel(model) ||
      normalizeModel(choice.label) === normalizeModel(model)
    );
  }

  function choiceMatchesProfile(choice, profile) {
    const effort = providerEffort(profile?.reasoningEffort);
    return Boolean(choice?.available && effort && choice.effort === effort && modelMatches(choice, profile));
  }

  async function selectedModelAndEffort(profile) {
    let snapshot = await readPickerState();
    if (!snapshot) {
      const ui = modelPickerAccess();
      snapshot = await ui.open();
      if (!snapshot) {
        ui.close();
        return null;
      }
      const choice = snapshot.choices.find((entry) => entry.bucket === snapshot.currentBucket);
      ui.close();
      return choiceMatchesProfile(choice, profile)
        ? { model: choice.id, reasoningEffort: choice.effort }
        : null;
    }

    if (snapshot.selected) {
      const expectedEffort = providerEffort(profile?.reasoningEffort);
      const requested = [profile?.modelKey, profile?.modelLabel].filter(Boolean);
      const modelConfirmed = requested.some((value) => snapshot.selected.id === value);
      return modelConfirmed && snapshot.selected.effort === expectedEffort
        ? { model: snapshot.selected.id, reasoningEffort: snapshot.selected.effort }
        : null;
    }

    const choice = snapshot.choices.find((entry) => entry.bucket === snapshot.currentBucket);
    return choiceMatchesProfile(choice, profile)
      ? { model: choice.id, reasoningEffort: choice.effort }
      : null;
  }

  async function restoreModelPicker(ui, original) {
    const version = await ui.version(original.version);
    if (!version) return false;
    const state = await ui.bucket(original.currentBucket);
    const previous = original.choices.find((choice) => choice.bucket === original.currentBucket);
    const selected = state?.choices.find((choice) => choice.bucket === state.currentBucket);
    return Boolean(
      previous &&
      selected?.id === previous.id &&
      selected?.effort === previous.effort
    );
  }

  async function inspectModelSettings() {
    const ui = modelPickerAccess();
    let original = await ui.open();
    if (!original) {
      ui.close();
      await sleep(200);
      original = await ui.open();
      if (!original) {
        ui.close();
        return null;
      }
    }

    for (let attempt = 0; attempt < 2; attempt += 1) {
      const result = new Map();
      let discoveryOk = true;
      let restored = false;
      try {
        for (const version of original.versions) {
          const state = await ui.version(version.id);
          if (!state) {
            discoveryOk = false;
            break;
          }
          for (const choice of state.choices.filter((entry) => entry.available)) {
            const existing = result.get(choice.familyId) || {
              id: choice.familyId,
              label: choice.familyLabel,
              efforts: [],
              aliases: [],
              choices: []
            };
            if (!existing.efforts.includes(choice.effort)) existing.efforts.push(choice.effort);
            if (!existing.aliases.includes(choice.id)) existing.aliases.push(choice.id);
            if (!existing.choices.some((entry) => entry.id === choice.id && entry.effort === choice.effort)) {
              existing.choices.push({ id: choice.id, effort: choice.effort });
            }
            result.set(choice.familyId, existing);
          }
        }
      } finally {
        restored = await restoreModelPicker(ui, original);
        ui.close();
      }

      if (discoveryOk && restored && result.size) return [...result.values()];
      if (!restored || attempt === 1) return null;

      // A discovery pass can race ChatGPT's picker hydration immediately after the
      // popup opens. Retry once only after the exact original selection was restored.
      await sleep(200);
      const reopened = await ui.open();
      if (!reopened) {
        ui.close();
        return null;
      }
      const previous = original.choices.find((choice) => choice.bucket === original.currentBucket);
      const selected = reopened.choices.find((choice) => choice.bucket === reopened.currentBucket);
      if (
        reopened.version !== original.version ||
        reopened.currentBucket !== original.currentBucket ||
        !previous ||
        selected?.id !== previous.id ||
        selected?.effort !== previous.effort
      ) {
        await restoreModelPicker(ui, original);
        ui.close();
        return null;
      }
    }
    return null;
  }

  async function selectModelSettings(profile) {
    const desiredEffort = providerEffort(profile?.reasoningEffort);
    if (!desiredEffort) return false;
    const ui = modelPickerAccess();
    const original = await ui.open();
    if (!original) {
      ui.close();
      return false;
    }

    let selected = false;
    try {
      const currentVersion = original.versions.find((version) => version.id === original.version);
      const versions = [currentVersion, ...original.versions.filter((version) => version.id !== original.version)].filter(Boolean);
      for (const version of versions) {
        const state = await ui.version(version.id);
        if (!state) return false;
        const choices = state.choices.filter((choice) => choiceMatchesProfile(choice, profile));
        if (!choices.length) continue;
        const choice = choices.find((entry) => entry.bucket === state.currentBucket) || choices[0];
        const after = await ui.bucket(choice.bucket);
        const confirmed = after?.choices.find((entry) => entry.bucket === after.currentBucket);
        selected = Boolean(
          confirmed?.available === true &&
          confirmed.id === choice.id &&
          confirmed.effort === desiredEffort &&
          modelMatches(confirmed, profile)
        );
        return selected;
      }
      return false;
    } finally {
      if (!selected) {
        const restoredVersion = await ui.version(original.version);
        if (restoredVersion) await ui.bucket(original.currentBucket);
      }
      ui.close();
    }
  }

  function composerText(box = composer()) {
    if (!box) return '';
    return typeof box.innerText === 'string' ? box.innerText.trim() : (box.textContent || '').trim();
  }

  function insertPrompt(value) {
    const box = composer();
    if (!visible(box) || generating() || composerText(box) || typeof value !== 'string' || !value) return false;
    try {
      box.focus();
      const selection = document.getSelection();
      if (!selection) return false;
      selection.selectAllChildren(box);
      const fragment = document.createElement('p');
      value.split('\n').forEach((line, index) => {
        if (index) fragment.append(document.createElement('br'));
        fragment.append(document.createTextNode(line));
      });
      if (!document.execCommand('insertHTML', false, fragment.innerHTML)) return false;
      box.dispatchEvent(new InputEvent('input', {
        bubbles: true,
        inputType: 'insertText',
        data: value
      }));
      return compact(composerText(box)) === compact(value);
    } catch {
      return false;
    }
  }

  function workerEvidence(marker) {
    return {
      conversationId: conversationIdFromPath(),
      conversationUrl: location.href,
      projectId: projectIdFromPath(),
      markerPresent: taskMarkerPresent(marker),
      generating: generating(),
      userTurnCount: userMessages().length,
      assistantTurnCount: assistantMessages().length,
      composerEmpty: composerText().length === 0
    };
  }

  async function commitSendOnce(expectedPrompt, marker) {
    const box = composer();
    if (!box || generating()) return { state: 'failed', reason: 'composer_not_ready' };
    if (compact(composerText(box)) !== compact(expectedPrompt)) {
      return { state: 'failed', reason: 'prepared_prompt_changed' };
    }
    const button = await waitFor(() => {
      const value = sendButton();
      return value && !value.disabled && value.getAttribute('aria-disabled') !== 'true' ? value : null;
    }, 8000);
    if (!button) return { state: 'failed', reason: 'send_button_unavailable' };

    const baseline = {
      sourceUrl: location.href,
      conversationId: conversationIdFromPath(),
      projectId: projectIdFromPath(),
      userTurnCount: userMessages().length,
      assistantTurnCount: assistantMessages().length,
      markerPresent: taskMarkerPresent(marker),
      composerHadPrompt: true
    };
    setTimeout(() => {
      try {
        if (
          visible(button) &&
          !generating() &&
          compact(composerText()) === compact(expectedPrompt) &&
          !button.disabled &&
          button.getAttribute('aria-disabled') !== 'true'
        ) {
          button.click();
        }
      } catch {}
    }, 50);
    return { state: 'committed', baseline };
  }

  function context() {
    const projectId = projectIdFromPath();
    if (!projectId) {
      return {
        projectId: null,
        sourceUrl: location.href,
        conversationId: conversationIdFromPath(),
        projectUrl: null,
        generating: generating()
      };
    }
    const link = projectLink(projectId);
    return {
      projectId,
      sourceUrl: location.href,
      conversationId: conversationIdFromPath(),
      projectUrl: link?.href || projectHomeUrl(),
      generating: generating()
    };
  }

  window.MOONDESK_CHATGPT_DOM = {
    context,
    correlationEvidence: readCorrelationEvidence,
    projectIdFromPath,
    conversationIdFromPath,
    enterProject,
    taskMarkerPresent,
    inspectModelSettings,
    selectModelSettings,
    selectedModelAndEffort,
    insertPrompt,
    workerEvidence,
    commitSendOnce,
    composerReady: () => visible(composer()) && !generating()
  };
})();
