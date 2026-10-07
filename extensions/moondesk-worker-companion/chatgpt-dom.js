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

  const LEGACY_TURN = 'section[data-testid^="conversation-turn"]';
  const SHELL_TURN = '[data-app-shell-main-surface] [data-thread-find-target="conversation"] [data-turn-key]';
  const SEARCH_TURN = '[data-chatgpt-search-unit-key]';
  const TURN = `${LEGACY_TURN}, ${SHELL_TURN}, ${SEARCH_TURN}`;
  const PICKER = '[data-testid="composer-intelligence-picker-content"], [data-model-picker-view]';

  function onKeptPage(node) {
    for (let page = node?.closest?.('[data-app-shell-page-surface]'); page;
      page = page.parentElement?.closest?.('[data-app-shell-page-surface]')) {
      if (getComputedStyle(page).display === 'none') return true;
    }
    return false;
  }

  function composerCssVisible(node) {
    try {
      if (!node || onKeptPage(node)) return false;
      for (let parent = node; parent; parent = parent.parentElement) {
        const style = getComputedStyle(parent);
        if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return false;
      }
      return true;
    } catch {
      return false;
    }
  }

  function composer() {
    const classic = [...document.querySelectorAll('#prompt-textarea')].filter(composerCssVisible);
    if (classic.length) return classic.length === 1 ? classic[0] : null;
    const candidates = [...document.querySelectorAll(
      'form[data-chatgpt-composer] [contenteditable="true"][role="textbox"], ' +
      'form [data-composer-markdown][contenteditable="true"][role="textbox"]'
    )].filter((node) =>
      !node.closest(`${TURN},.markdown,[hidden],[aria-hidden="true"],[inert]`) && composerCssVisible(node)
    );
    return candidates.length === 1 ? candidates[0] : null;
  }

  const composerForm = () => composer()?.closest('form') || null;
  const composerWritable = () => {
    const box = composer();
    return Boolean(
      box?.isConnected &&
      box.getAttribute('aria-disabled') !== 'true' &&
      box.getAttribute('contenteditable') !== 'false'
    );
  };
  const composerSubmitReady = () => Boolean(
    composerWritable() && !generating() && !composerText().trim()
  );

  function renderedComposerNode(node) {
    if (!node?.isConnected || node.closest(`${TURN},[data-message-author-role],[hidden],[aria-hidden="true"],[inert]`)) {
      return false;
    }
    for (let parent = node; parent; parent = parent.parentElement) {
      const style = getComputedStyle(parent);
      if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse') return false;
    }
    return true;
  }

  function nativeComposerControls(selector) {
    const form = composerForm();
    return [...(form || document).querySelectorAll(selector)].filter((button) =>
      renderedComposerNode(button) && (!form || button.closest('form') === form)
    );
  }

  const STOP_SQUARE = /^\s*M4\.5 5\.75/;
  function primarySlotControls() {
    const form = composerForm();
    if (!form) return [];
    return [...form.querySelectorAll('button[class*="size-token-button-composer"][class*="bg-composer-primary"]')]
      .filter((button) => renderedComposerNode(button) && button.closest('form') === form);
  }

  function isStopSquare(button) {
    if (!button || button.hasAttribute('data-state')) return false;
    const paths = button.querySelectorAll('svg path');
    return paths.length === 1 && STOP_SQUARE.test(paths[0].getAttribute('d') || '');
  }

  function stopControls() {
    const labelled = nativeComposerControls(
      'button[data-testid="stop-button"],button[data-testid="composer-stop-button"],' +
      'button[aria-label="Stop streaming"],button[aria-label="Stop generating"],button[aria-label="Stop answering"]'
    );
    return labelled.length ? labelled : primarySlotControls().filter(isStopSquare);
  }

  function generating() {
    if (stopControls().length) return true;
    return false;
  }

  function localeFreeSendControls() {
    const box = composer();
    const form = box?.closest('form');
    if (!form) return [];
    const drafted = (typeof box.innerText === 'string' ? box.innerText : box.textContent || '').trim();
    if (!drafted || generating()) return [];
    return primarySlotControls().filter((button) => {
      if (button.hasAttribute('data-state')) return false;
      const paths = button.querySelectorAll('svg path');
      if (paths.length === 1 && STOP_SQUARE.test(paths[0].getAttribute('d') || '')) return false;
      return paths.length >= 1 && paths.length <= 2;
    });
  }

  function sendButton() {
    const labelled = nativeComposerControls(
      'button[data-testid="send-button"],button[data-testid="composer-submit-button"],' +
      'form button[aria-label^="Send" i],form[data-chatgpt-composer] button[type="submit"]'
    );
    const buttons = labelled.length ? labelled : localeFreeSendControls();
    return buttons.length === 1 ? buttons[0] : null;
  }

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

  const projectHomeId = (pathname = location.pathname) =>
    /^\/g\/(g-p-[0-9a-f]{32})(?:-[^/]+)?\/project\/?$/i.exec(pathname)?.[1]?.toLowerCase() || null;
  const projectHome = () => Boolean(projectHomeId());

  function projectHomeUrl() {
    const projectId = projectIdFromPath();
    if (!projectId) return null;
    const match = /^\/g\/([^/]+)/i.exec(location.pathname);
    if (!match) return null;
    return location.origin + '/g/' + match[1] + '/project';
  }

  function projectLink(projectId) {
    const matched = [...document.querySelectorAll('a[href]')].filter((link) => {
      try {
        if (link.closest(TURN) || !composerCssVisible(link)) return false;
        const url = new URL(link.href, location.href);
        return url.origin === location.origin && projectHomeId(url.pathname) === projectId;
      } catch {
        return false;
      }
    });
    return matched.length === 1 ? matched[0] : null;
  }

  function waitForComposerReady(timeoutMs = 15000, stillCurrent = () => true) {
    if (!stillCurrent()) return Promise.resolve(false);
    if (composerWritable() && !generating()) return Promise.resolve(true);
    return new Promise((resolve) => {
      let done = false;
      const finish = (value) => {
        if (done) return;
        done = true;
        observer.disconnect();
        clearTimeout(timer);
        resolve(value);
      };
      const check = () => {
        if (!stillCurrent()) return finish(false);
        if (composerWritable() && !generating()) finish(true);
      };
      const observer = new MutationObserver(check);
      observer.observe(document.documentElement, {
        childList: true,
        subtree: true,
        attributes: true,
        attributeFilter: [
          'class', 'style', 'hidden', 'aria-hidden', 'inert', 'contenteditable', 'aria-disabled',
          'data-chatgpt-composer', 'data-composer-markdown'
        ]
      });
      const timer = setTimeout(() => finish(false), timeoutMs);
      check();
    });
  }

  function currentTurnNodes() {
    return [...document.querySelectorAll(TURN)].filter((node) =>
      !onKeptPage(node) && !node.closest('.markdown,[contenteditable]')
    );
  }

  async function enterProject(projectId, stillCurrent = () => true) {
    const sourceConversation = conversationIdFromPath();
    if (!/^g-p-[0-9a-f]{32}$/.test(projectId || '')) return false;
    if (projectHomeId() === projectId) {
      return Boolean(await waitForComposerReady(15000, stillCurrent)) && !conversationIdFromPath();
    }
    if (!sourceConversation) return false;

    return new Promise((resolve) => {
      let clicked = false;
      let done = false;
      let timer = null;
      const finish = (value) => {
        if (done) return;
        done = true;
        observer.disconnect();
        clearTimeout(timer);
        resolve(value);
      };
      const check = () => {
        if (done) return;
        if (!stillCurrent()) return finish(false);
        if (
          clicked &&
          projectHomeId() === projectId &&
          !conversationIdFromPath() &&
          composerSubmitReady() &&
          currentTurnNodes().length === 0
        ) return finish(true);
        if (conversationIdFromPath() !== sourceConversation) {
          if (projectHomeId() !== projectId) finish(false);
          return;
        }
        if (clicked || !composerSubmitReady()) return;
        const link = projectLink(projectId);
        if (!link) return;
        clicked = true;
        clearTimeout(timer);
        timer = setTimeout(() => finish(false), 12000);
        link.click();
        check();
      };
      const observer = new MutationObserver(check);
      observer.observe(document.documentElement, { childList: true, subtree: true, attributes: true });
      timer = setTimeout(() => finish(false), 60000);
      check();
    });
  }

  function messageNodes(role) {
    const selectors = [
      `[data-message-author-role="${role}"]`,
      `[data-content-search-unit-key$=":${role}"]`,
      `[data-chatgpt-search-unit-key$=":${role}"]`,
      role === 'assistant' ? '[data-chatgpt-agent-turn-start]' : null
    ].filter(Boolean).join(',');
    const found = [...document.querySelectorAll(selectors)].filter((node) =>
      !onKeptPage(node) && composerCssVisible(node)
    );
    return [...new Set(found)];
  }

  const userMessages = () => messageNodes('user');
  const assistantMessages = () => messageNodes('assistant');

  const taskMarkerPresent = (marker) => userMessages().some(
    (node) => (node.innerText || node.textContent || '').includes(marker)
  );

  function modelPickerTrigger() {
    const root = composerForm();
    if (!root) return null;
    const reported = '[data-codex-intelligence-trigger],[data-composer-navigation-target="reasoning"]';
    const candidates = [...new Set([
      ...root.querySelectorAll('button[aria-haspopup="menu"]'),
      ...document.querySelectorAll(reported)
    ])].filter(
      (node) =>
        node.matches?.('button,[role="button"]') &&
        visible(node) &&
        !node.closest(`${TURN},[data-message-author-role],.markdown,[contenteditable]`) &&
        node.id !== 'composer-plus-btn' &&
        node.getAttribute('data-testid') !== 'composer-plus-btn'
    );
    const owned = candidates.filter((node) =>
      node.getAttribute('data-moondesk-picker-route') === location.pathname
    );
    if (owned.length === 1) return owned[0];
    // Legacy provider pickers without the new reported anchors may still be uniquely identifiable.
    return candidates.length === 1 && !candidates[0].matches(reported) ? candidates[0] : null;
  }

  const pickerRoot = () => document.querySelector(PICKER);

  function providerEfforts(reasoningEffort) {
    return ({
      instant: ['none'],
      minimal: ['minimal'],
      low: ['low'],
      medium: ['medium'],
      high: ['high'],
      // Legacy saved Extra High profiles remain valid across the provider's xhigh -> max migration.
      extra_high: ['xhigh', 'max'],
      max: ['max'],
      ultra: ['ultra'],
      pro: ['pro']
    })[reasoningEffort] || [];
  }

  function resolveWantedEffort(desiredEfforts, offeredEfforts) {
    const offered = [...new Set(offeredEfforts.filter((effort) => PROVIDER_EFFORTS.has(effort)))];
    const exact = desiredEfforts.find((effort) => offered.includes(effort));
    if (exact) return exact;
    const ladder = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'];
    const target = ladder.indexOf(desiredEfforts[0]);
    const candidates = offered.filter((effort) => ladder.includes(effort));
    if (target < 0 || !candidates.length) return null;
    candidates.sort((left, right) => {
      const leftDistance = Math.abs(ladder.indexOf(left) - target);
      const rightDistance = Math.abs(ladder.indexOf(right) - target);
      return leftDistance - rightDistance || ladder.indexOf(right) - ladder.indexOf(left);
    });
    return candidates[0];
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

  async function prepareChatModelSurface(stillCurrent = () => true) {
    const radios = () => [...document.querySelectorAll('[role="radio"][data-tpp-toggle-value]')].filter(visible);
    const state = () => {
      const nodes = radios();
      const chat = nodes.filter((node) => node.getAttribute('data-tpp-toggle-value') === 'chatgpt');
      const work = nodes.filter((node) => node.getAttribute('data-tpp-toggle-value') === 'work');
      return chat.length === 1 && work.length === 1 ? { chat: chat[0], work: work[0] } : null;
    };

    if (!stillCurrent()) return false;
    const before = state();
    if (!before) return radios().length === 0 && stillCurrent();
    if (before.chat.getAttribute('aria-checked') === 'true') return true;
    if (
      before.work.getAttribute('aria-checked') !== 'true' ||
      before.chat.disabled ||
      before.chat.getAttribute('aria-disabled') === 'true'
    ) {
      return false;
    }
    return new Promise((resolve) => {
      let done = false;
      const finish = (value) => {
        if (done) return;
        done = true;
        observer.disconnect();
        clearTimeout(timer);
        resolve(value);
      };
      const check = () => {
        if (!stillCurrent()) return finish(false);
        const next = state();
        if (
          next?.chat.getAttribute('aria-checked') === 'true' &&
          next.work.getAttribute('aria-checked') === 'false'
        ) finish(true);
      };
      const observer = new MutationObserver(check);
      observer.observe(document.documentElement, { subtree: true, childList: true, attributes: true });
      const timer = setTimeout(() => finish(false), 5000);
      before.chat.click();
      check();
    });
  }

  function pickerVersionNamed(row, expected) {
    const text = (node) => compact(node?.textContent);
    const name = compact(expected);
    for (let node = row, depth = 0; node && depth < 8; depth += 1) {
      if (text(node) === name) return true;
      node = [...(node.childNodes || [])].find((child) =>
        text(child) &&
        (child.nodeType === Node.TEXT_NODE ||
          (child.nodeType === Node.ELEMENT_NODE && !child.matches?.('svg,[hidden],[aria-hidden="true"],[inert]')))
      );
    }
    return false;
  }

  function modelPickerAccess(stillCurrent = () => true) {
    let motion = null;
    const shown = (node) => visible(node);
    const openPicker = () => {
      const panel = pickerRoot();
      return panel && panel.closest('[role="menu"],[role="dialog"]')?.getAttribute('data-state') !== 'closed'
        ? panel
        : null;
    };
    const wait = (read, timeoutMs = 3500) => new Promise((resolve) => {
      let reading = false;
      let dirty = false;
      let done = false;
      const finish = (value) => {
        if (done) return;
        done = true;
        observer.disconnect();
        clearTimeout(timer);
        resolve(value);
      };
      const check = async () => {
        if (done) return;
        if (!stillCurrent()) return finish(null);
        if (reading) {
          dirty = true;
          return;
        }
        reading = true;
        try {
          let value = read();
          if (value?.then) value = await value;
          if (stillCurrent() && value) finish(value);
        } catch {
          finish(null);
        } finally {
          reading = false;
          if (dirty && !done) {
            dirty = false;
            void check();
          }
        }
      };
      const observer = new MutationObserver(check);
      observer.observe(document.documentElement, {
        subtree: true,
        childList: true,
        attributes: true,
        characterData: true
      });
      const timer = setTimeout(() => finish(null), timeoutMs);
      void check();
    });
    const state = (predicate = null, timeoutMs = 3500) => wait(async () => {
      const value = await readPickerState();
      if (!value || value.selected) return null;
      return !predicate || predicate(value) ? value : null;
    }, timeoutMs);
    const readyTrigger = () => wait(async () => {
      await readPickerState();
      return modelPickerTrigger();
    }, 15000);

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
        if (!stillCurrent()) return null;
        motion = document.createElement('style');
        motion.textContent =
          '[role="menu"]:has(> [data-testid="composer-intelligence-picker-content"]),' +
          '[role="dialog"]:has([data-testid="composer-intelligence-picker-content"]),' +
          '[role="menu"]:has([data-model-picker-view]),[role="menu"][data-model-picker-view]{animation:none!important}';
        document.head.append(motion);
        const trigger = await readyTrigger();
        if (!trigger || !(await prepareChatModelSurface(stillCurrent))) return null;
        if (!openPicker()) {
          const current = await readyTrigger();
          if (!key(current, 'Enter') || !(await wait(openPicker))) return null;
        }
        return state();
      },
      async close() {
        try {
          if (!stillCurrent()) return false;
          const panel = pickerRoot();
          if (!panel) return true;
          const dialog = panel.closest('[role="dialog"]');
          const active = document.activeElement;
          if (!shown(panel) && !shown(dialog)) return true;
          const target = panel.contains(active) || dialog?.contains(active) ? active : panel;
          if (!key(target, 'Escape')) return false;
          const closed = Boolean(await wait(() =>
            !shown(pickerRoot()) && (!dialog?.isConnected || !shown(dialog))
          ));
          if (closed && stillCurrent()) await readPickerState();
          return closed && stillCurrent();
        } finally {
          motion?.remove();
          motion = null;
        }
      },
      async version(versionId) {
        const before = await state();
        if (!before) return null;
        const versionRows = () => [...(pickerRoot()?.querySelectorAll('[role="menuitemradio"]') || [])].filter(visible);
        if (before.version === versionId && !versionRows().length) return before;
        const versionLabel = before.versions.find((version) => version.id === versionId)?.label;
        if (!versionLabel) return null;

        if (!versionRows().length) {
          const toggles = [...pickerRoot().querySelectorAll(
            '[role="menuitem"][aria-expanded], [role="menuitem"][data-model-picker-view-toggle]'
          )].filter(visible);
          if (toggles.length !== 1) return null;
          toggles[0].click();
        }
        const option = await wait(() => {
          const rows = versionRows().filter((node) =>
            node.getAttribute('aria-disabled') !== 'true' && pickerVersionNamed(node, versionLabel)
          );
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

  function legacyDisplayFamilyMatches(choice, profile) {
    const requested = [profile?.modelKey, profile?.modelLabel]
      .filter((value) => typeof value === 'string' && value.trim())
      .map(normalizeModel);
    // GPT-5.6 Sol is the product/display family. ChatGPT's live picker uses execution ids
    // `gpt-5-6` (Instant) and `gpt-5-6-thinking` (reasoning); `gpt-5-6-pro` is a different
    // family and must never be admitted by this compatibility rule. This mirrors CoS's
    // account-observed stale-default resolution without turning arbitrary labels into aliases.
    const wantsGpt56Sol = requested.some((value) =>
      value === 'gpt5.6sol' || value === '5.6sol' || value === 'sol'
    );
    if (!wantsGpt56Sol) return false;
    const executionId = String(choice?.id || '').trim().toLowerCase();
    return executionId === 'gpt-5-6' || executionId === 'gpt-5-6-thinking';
  }

  function modelRank(choice, profile) {
    const requested = [profile?.modelKey, profile?.modelLabel].filter(
      (value) => typeof value === 'string' && value.trim()
    );
    if (requested.some((model) => choice.familyId === model || choice.id === model)) return 2;
    if (requested.some((model) =>
      normalizeModel(choice.familyLabel) === normalizeModel(model) ||
      normalizeModel(choice.label) === normalizeModel(model)
    )) return 1;
    return legacyDisplayFamilyMatches(choice, profile) ? 1 : 0;
  }

  function modelMatches(choice, profile) {
    return modelRank(choice, profile) > 0;
  }

  function choiceMatchesResolvedProfile(state, choice, profile) {
    if (!choice?.available || !modelMatches(choice, profile)) return false;
    const desired = providerEfforts(profile?.reasoningEffort);
    if (!desired.length) return false;
    const modelChoices = state.choices.filter((entry) => entry.available && modelRank(entry, profile) > 0);
    const bestRank = Math.max(0, ...modelChoices.map((entry) => modelRank(entry, profile)));
    const offered = modelChoices.filter((entry) => modelRank(entry, profile) === bestRank);
    if (!offered.length || new Set(offered.map((entry) => entry.familyId)).size !== 1) return false;
    const wanted = resolveWantedEffort(desired, offered.map((entry) => entry.effort));
    return Boolean(
      wanted &&
      modelRank(choice, profile) === bestRank &&
      choice.familyId === offered[0].familyId &&
      choice.effort === wanted
    );
  }

  function visibleModelSelection(profile = null) {
    const node = pickerRoot() || modelPickerTrigger();
    if (node?.getAttribute('data-moondesk-selected-route') !== location.pathname) return null;
    const model = node.getAttribute('data-moondesk-selected-model');
    const reasoningEffort = node.getAttribute('data-moondesk-selected-effort');
    if (
      typeof model !== 'string' ||
      !/^[a-zA-Z0-9._-]{1,80}$/.test(model) ||
      !PROVIDER_EFFORTS.has(reasoningEffort)
    ) return null;
    if (profile) {
      const desired = providerEfforts(profile.reasoningEffort);
      const requested = [profile.modelKey, profile.modelLabel].filter(Boolean);
      if (!desired.includes(reasoningEffort) || !requested.some((value) => value === model)) return null;
    }
    return { model, reasoningEffort };
  }

  async function selectedModelAndEffort(profile) {
    const fromFullState = (state) => {
      const choice = state?.choices?.find((entry) => entry.bucket === state.currentBucket);
      return choiceMatchesResolvedProfile(state, choice, profile)
        ? { model: choice.id, reasoningEffort: choice.effort }
        : null;
    };

    const snapshot = await readPickerState();
    if (snapshot && !snapshot.selected) return fromFullState(snapshot);

    if (snapshot?.selected) {
      const expectedEfforts = providerEfforts(profile?.reasoningEffort);
      const requested = [profile?.modelKey, profile?.modelLabel].filter(Boolean);
      const modelConfirmed = requested.some((value) => snapshot.selected.id === value);
      if (modelConfirmed && expectedEfforts.includes(snapshot.selected.effort)) {
        return { model: snapshot.selected.id, reasoningEffort: snapshot.selected.effort };
      }
      // A closed picker exposes only the execution id. Older saved profiles can legitimately name
      // the same provider family by a previous slug/display identity, so reopen the native picker
      // and require full account-evaluated family proof before rejecting the readback.
    }

    const ui = modelPickerAccess();
    const full = await ui.open();
    if (!full) {
      await ui.close();
      return null;
    }
    const confirmed = fromFullState(full);
    const closed = await ui.close();
    return closed ? confirmed : null;
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

  function collectModelChoices(result, state) {
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

  async function inspectModelSettings(stillCurrent = () => true, failure = () => {}) {
    const ui = modelPickerAccess(stillCurrent);
    const original = await ui.open();
    if (!original) {
      await ui.close();
      failure('picker_unavailable');
      return null;
    }

    const result = new Map();
    let restored = false;
    let closed = false;
    try {
      // Read each account-evaluated version once. The provider-owned choices already include
      // availability/denial state, so walking every effort as a separate mutation only makes
      // discovery slower and more fragile on the current shell.
      for (const version of original.versions) {
        if (!stillCurrent()) throw new Error('catalog_deadline');
        const state = await ui.version(version.id);
        if (!state) throw new Error('model_unconfirmed');
        collectModelChoices(result, state);
      }
    } catch (error) {
      failure(error?.message === 'catalog_deadline' ? 'catalog_deadline' : 'model_unconfirmed');
      result.clear();
    } finally {
      if (stillCurrent()) restored = await restoreModelPicker(ui, original);
      closed = await ui.close();
    }

    if (!restored) failure('restore_failed');
    if (!closed) failure('picker_close_failed');
    return restored && closed && stillCurrent() && result.size ? [...result.values()] : null;
  }

  async function selectModelSettings(profile, failure = () => {}, stillCurrent = () => true) {
    let failureCode = null;
    const fail = (code) => {
      if (!failureCode) failureCode = code;
      return false;
    };
    const publishFailure = () => {
      try { failure(failureCode || 'selection_unconfirmed'); } catch {}
      return false;
    };

    const desiredEfforts = providerEfforts(profile?.reasoningEffort);
    if (!desiredEfforts.length) {
      fail('effort_invalid');
      return publishFailure();
    }
    if (!stillCurrent()) {
      fail('launch_target_changed');
      return publishFailure();
    }
    const ui = modelPickerAccess(stillCurrent);
    const original = await ui.open();
    if (!original) {
      await ui.close();
      fail('picker_unavailable');
      return publishFailure();
    }

    let selected = false;
    let closed = false;
    try {
      const current = original.choices.find((choice) => choice.bucket === original.currentBucket);
      if (
        current?.available &&
        modelRank(current, profile) > 0 &&
        desiredEfforts.includes(current.effort)
      ) {
        selected = true;
      } else {
        const currentVersion = original.versions.find((version) => version.id === original.version);
        const versions = [currentVersion, ...original.versions.filter((version) => version.id !== original.version)].filter(Boolean);
        const offered = [];
        for (const version of versions) {
          const state = await ui.version(version.id);
          if (!state) {
            fail('version_unconfirmed');
            break;
          }
          for (const choice of state.choices) {
            const rank = choice.available ? modelRank(choice, profile) : 0;
            if (rank) offered.push({ version: version.id, choice, rank });
          }
        }
        if (!failureCode && !offered.length) fail('model_unavailable');

        let wantedEffort = null;
        if (!failureCode) {
          wantedEffort = desiredEfforts.find((effort) =>
            offered.some((entry) => entry.choice.effort === effort)
          ) || null;
          if (!wantedEffort) {
            const ladder = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'];
            const target = ladder.indexOf(desiredEfforts[0]);
            const efforts = [...new Set(offered.map((entry) => entry.choice.effort))]
              .filter((effort) => ladder.includes(effort));
            if (target >= 0 && efforts.length) {
              efforts.sort((left, right) => {
                const leftDistance = Math.abs(ladder.indexOf(left) - target);
                const rightDistance = Math.abs(ladder.indexOf(right) - target);
                return leftDistance - rightDistance || ladder.indexOf(right) - ladder.indexOf(left);
              });
              wantedEffort = efforts[0];
            }
          }
          if (!wantedEffort) fail('effort_unavailable');
        }

        let wanted = null;
        if (!failureCode) {
          const candidates = offered.filter((entry) => entry.choice.effort === wantedEffort);
          const rank = Math.max(0, ...candidates.map((candidate) => candidate.rank));
          const matches = candidates.filter((candidate) => candidate.rank === rank);
          if (
            !matches.length ||
            new Set(matches.map((candidate) => candidate.choice.familyId)).size !== 1 ||
            new Set(matches.map((candidate) => `${candidate.choice.id}\u0000${candidate.choice.effort}`)).size !== 1
          ) {
            fail('candidate_ambiguous');
          } else {
            wanted = matches.find((candidate) =>
              candidate.version === original.version && candidate.choice.bucket === original.currentBucket
            ) || matches[0];
          }
        }

        if (!failureCode && wanted) {
          const state = await ui.version(wanted.version);
          const choice = wanted.choice;
          if (!state?.choices.some((next) =>
            next.bucket === choice.bucket &&
            next.available &&
            next.id === choice.id &&
            next.effort === choice.effort
          )) {
            fail('choice_stale');
          } else {
            const after = await ui.bucket(choice.bucket);
            if (!after) {
              fail('bucket_unconfirmed');
            } else {
              const confirmed = after.choices.find((entry) => entry.bucket === after.currentBucket);
              selected = Boolean(
                confirmed?.available === true &&
                confirmed.id === choice.id &&
                confirmed.effort === choice.effort &&
                modelRank(confirmed, profile) > 0
              );
              if (!selected) fail('selection_unconfirmed');
            }
          }
        }
      }
    } finally {
      if (!selected) {
        const restoredVersion = await ui.version(original.version);
        const restored = restoredVersion ? await ui.bucket(original.currentBucket) : null;
        if (!restored && !failureCode) fail('restore_failed');
      }
      closed = await ui.close();
    }

    if (!selected) return publishFailure();
    if (!closed) {
      fail('picker_close_failed');
      return publishFailure();
    }
    return true;
  }

  function composerText(box = composer()) {
    if (!box) return '';
    return typeof box.innerText === 'string' ? box.innerText.trim() : (box.textContent || '').trim();
  }

  function preparedPromptMatches(value) {
    return Boolean(
      typeof value === 'string' &&
      value &&
      composerWritable() &&
      !generating() &&
      compact(composerText()) === compact(value)
    );
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

  async function commitSendOnce(expectedPrompt, marker, stillCurrent = () => true) {
    if (!stillCurrent()) return { state: 'failed', reason: 'prepared_launch_target_changed' };
    let box = composer();
    if (!box || !composerWritable() || generating()) return { state: 'failed', reason: 'composer_not_ready' };
    if (compact(composerText(box)) !== compact(expectedPrompt)) {
      return { state: 'failed', reason: 'prepared_prompt_changed' };
    }
    const routeChanged = {};
    const button = await waitFor(() => {
      if (!stillCurrent()) return routeChanged;
      const value = sendButton();
      return value && !value.disabled && value.getAttribute('aria-disabled') !== 'true' ? value : null;
    }, 8000);
    if (button === routeChanged) return { state: 'failed', reason: 'prepared_launch_target_changed' };
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

    // Let synchronous React input work settle, then re-prove both the exact draft and native
    // control at the irreversible boundary. Returning "committed" before a delayed timer click
    // made the background believe Send happened even when a hidden tab throttled that timer.
    await Promise.resolve();
    box = composer();
    const currentButton = sendButton();
    if (!stillCurrent()) return { state: 'failed', reason: 'prepared_launch_target_changed' };
    if (
      !box ||
      !composerWritable() ||
      generating() ||
      compact(composerText(box)) !== compact(expectedPrompt) ||
      !currentButton ||
      currentButton !== button ||
      currentButton.disabled ||
      currentButton.getAttribute('aria-disabled') === 'true'
    ) {
      return { state: 'failed', reason: 'prepared_prompt_changed_before_send' };
    }
    currentButton.click();
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
    projectHomeId,
    conversationIdFromPath,
    enterProject,
    taskMarkerPresent,
    inspectModelSettings,
    selectModelSettings,
    visibleModelSelection,
    selectedModelAndEffort,
    preparedPromptMatches,
    insertPrompt,
    workerEvidence,
    commitSendOnce,
    waitForComposerReady,
    composerReady: () => composerWritable() && !generating()
  };
})();
