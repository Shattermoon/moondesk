(() => {
  if (window.__MOONDESK_WORKER_MODEL_STATE_MAIN__) return;
  window.__MOONDESK_WORKER_MODEL_STATE_MAIN__ = true;

  const ASK = 'moondesk-picker-ask';
  const REPLY = 'moondesk-picker-reply';
  const CORRELATION_ASK = 'moondesk-correlation-ask';
  const CORRELATION_REPLY = 'moondesk-correlation-reply';
  const MAX_CLIMB = 80;
  // React's DOM Fiber pointer can retain the previous render. ChatGPT's tree has exceeded
  // 400 levels in the live shell, so first prove the committed root and rebuild only that
  // exact child path before reading picker/correlation state.
  const MAX_ROOT_DEPTH = 2048;
  const MAX_ROOT_CHILD_VISITS = 4096;
  const MAX_CORRELATION_TURNS = 16;
  const MAX_CORRELATION_IDS = 32;

  // Cache reconstructed path views only within one synchronous helper request. Never retain a
  // React tree across page mutations, and never rewrite React's own return pointers.
  let currentPaths = null;

  function committedPath(fiber) {
    if (!fiber || typeof fiber !== 'object') return null;
    const path = [];
    const seen = new Set();
    let at = fiber;
    let base = null;
    let paired = false;
    const view = (node, parent) => ({
      memoizedProps: node.memoizedProps,
      memoizedState: node.memoizedState,
      updateQueue: node.updateQueue,
      return: parent
    });
    const remember = (node, value) => {
      currentPaths?.set(node, value);
      if (node.alternate && typeof node.alternate === 'object') currentPaths?.set(node.alternate, value);
    };

    while (at && path.length < MAX_ROOT_DEPTH) {
      if (seen.has(at)) return null;
      seen.add(at);
      const cached = currentPaths?.get(at);
      if (cached && cached.root.current === cached.current) {
        base = cached;
        break;
      }
      paired ||= Boolean(at.alternate);
      if (at.tag === 3) {
        const root = at.stateNode;
        const current = root?.current;
        if (!current || (current !== at && current !== at.alternate)) return null;
        base = { root, current, node: current, view: view(current, null) };
        remember(at, base);
        break;
      }
      path.push(at);
      at = at.return;
    }

    // Older unpaired owner facades have no root bookkeeping. A double-buffered or actual React
    // tree must prove its committed root, including after unmount or interrupted renders.
    if (!base) {
      return !paired && !at && path.every((node) => typeof node.tag !== 'number') ? fiber : null;
    }

    let budget = MAX_ROOT_CHILD_VISITS;
    for (let index = path.length - 1; index >= 0; index -= 1) {
      const wanted = path[index];
      let selected = null;
      for (let child = base.node.child; child; child = child.sibling) {
        budget -= 1;
        if (budget < 0) return null;
        if (child !== wanted && child !== wanted.alternate) continue;
        if (selected) return null;
        selected = child;
      }
      if (!selected || base.root.current !== base.current) return null;
      base = { root: base.root, current: base.current, node: selected, view: view(selected, base.view) };
      remember(wanted, base);
    }
    return base.view;
  }

  function fiberOf(node) {
    if (!node) return null;
    for (const key in node) {
      if (key.startsWith('__reactFiber$')) return committedPath(node[key]);
    }
    return null;
  }

  function visible(node) {
    return Boolean(
      node &&
      node.isConnected &&
      !node.closest('[hidden],[aria-hidden="true"],[inert]') &&
      node.getClientRects().length > 0
    );
  }

  function conversationIdFromPath() {
    const match = /\/c\/([0-9a-f-]{16,64})(?:\/|$)/i.exec(location.pathname);
    return match?.[1]?.toLowerCase() || null;
  }

  function boundedString(value, max = 128) {
    return typeof value === 'string' && value.length > 0 && value.length <= max ? value : null;
  }

  function normalizedRequestId(value) {
    const raw = boundedString(value, 160);
    if (!raw) return null;
    const id = raw.split('/')[0].trim();
    return /^[a-z0-9_-]{1,100}$/i.test(id) ? id : null;
  }

  function turnMessagesOf(fiber) {
    for (let at = fiber, up = 0; at && up < MAX_CLIMB; up += 1, at = at.return) {
      const props = at.memoizedProps;
      if (!props || typeof props !== 'object') continue;
      const turn = props.turn;
      if (turn && typeof turn === 'object' && Array.isArray(turn.messages)) return turn.messages;
      if (Array.isArray(props.allMessages)) return props.allMessages;
    }
    return null;
  }

  function conversationEvidenceOf(fiber) {
    let found = null;
    const conversations = new Map();
    for (let at = fiber, up = 0; at && up < MAX_CLIMB; up += 1, at = at.return) {
      const props = at.memoizedProps;
      if (!props || typeof props !== 'object') continue;
      const turn = props.turn && typeof props.turn === 'object' ? props.turn : null;
      const conversation =
        props.conversation && typeof props.conversation === 'object' ? props.conversation : null;
      if (conversation && !conversations.has(conversation)) {
        let serverId = null;
        try {
          const value =
            typeof conversation.serverId$ === 'function' ? conversation.serverId$() : null;
          if (typeof value === 'string' && /^[0-9a-f-]{16,64}$/i.test(value)) {
            serverId = value.toLowerCase();
          }
        } catch {}
        conversations.set(conversation, serverId);
      }

      const values = [
        props.clientThreadId,
        props.conversationId,
        conversation?.id,
        conversations.get(conversation),
        turn?.clientThreadId,
        turn?.conversationId
      ];
      for (const value of values) {
        if (typeof value !== 'string' || value.startsWith('WEB:')) continue;
        if (!/^[0-9a-f-]{16,64}$/i.test(value)) continue;
        const normalized = value.toLowerCase();
        if (found && found !== normalized) return { conversationId: null, conflict: true };
        found = normalized;
      }
    }
    return { conversationId: found, conflict: false };
  }

  function correlationSnapshot() {
    const routeConversation = conversationIdFromPath();
    if (!routeConversation) return null;

    let sections;
    try {
      sections = [...new Set(document.querySelectorAll(
        'section[data-testid^="conversation-turn"], ' +
        '[data-app-shell-main-surface] [data-thread-find-target="conversation"] [data-turn-key], ' +
        '[data-chatgpt-search-unit-key]'
      ))];
    } catch {
      return null;
    }

    const requestIds = [];
    const seen = new Set();
    const first = Math.max(0, sections.length - MAX_CORRELATION_TURNS);
    for (let index = first; index < sections.length && requestIds.length < MAX_CORRELATION_IDS; index += 1) {
      const fiber = fiberOf(sections[index]);
      if (!fiber) continue;
      const evidence = conversationEvidenceOf(fiber);
      if (evidence.conflict || evidence.conversationId !== routeConversation) continue;
      const messages = turnMessagesOf(fiber);
      if (!Array.isArray(messages)) continue;
      for (const message of messages) {
        const metadata =
          message && typeof message === 'object' && message.metadata && typeof message.metadata === 'object'
            ? message.metadata
            : null;
        const requestId = normalizedRequestId(metadata?.request_id);
        if (!requestId || seen.has(requestId)) continue;
        seen.add(requestId);
        requestIds.push(requestId);
        if (requestIds.length >= MAX_CORRELATION_IDS) break;
      }
    }

    return requestIds.length ? { conversationId: routeConversation, requestIds } : null;
  }

  const PICKER_TRIGGER = '[data-codex-intelligence-trigger],[data-composer-navigation-target="reasoning"]';

  function modelPickerTriggers() {
    const composer = document.querySelector(
      '#prompt-textarea, form[data-chatgpt-composer] [contenteditable="true"][role="textbox"], ' +
      'form [data-composer-markdown][contenteditable="true"][role="textbox"]'
    );
    const form = composer?.closest?.('form');
    return [...new Set([
      ...(form?.querySelectorAll?.('button[aria-haspopup="menu"]') || []),
      ...document.querySelectorAll(PICKER_TRIGGER)
    ])].filter(
      (node) =>
        node?.matches?.('button,[role="button"]') &&
        visible(node) &&
        !node.closest?.(
          '[data-testid^="conversation-turn"],[data-message-author-role],.markdown,[contenteditable],[hidden],[aria-hidden="true"],[inert]'
        ) &&
        node.id !== 'composer-plus-btn' &&
        node.getAttribute('data-testid') !== 'composer-plus-btn'
    );
  }

  function strictId(value) {
    return typeof value === 'string' && /^[a-zA-Z0-9._-]{1,80}$/.test(value) ? value : null;
  }

  function groupId(value) {
    return typeof value === 'string' &&
      /^[a-zA-Z0-9._ -]{1,80}$/.test(value) &&
      value.trim() === value &&
      value.trim()
      ? value
      : null;
  }

  function label(value) {
    return typeof value === 'string' && value.trim().length > 0 && value.length <= 80
      ? value.trim()
      : null;
  }

  function effortOf(choice) {
    if (choice?.category?.modelLane === 'pro') return 'pro';
    if (['auto', 'instant'].includes(choice?.category?.modelLane)) return 'none';
    if (choice?.thinkingEffort === 'max' && choice?.modelConfig?.isWorkModeModel === true) {
      return 'max';
    }
    return ({
      min: 'low',
      standard: 'medium',
      extended: 'high',
      max: 'xhigh',
      minimal: 'minimal',
      low: 'low',
      medium: 'medium',
      high: 'high',
      xhigh: 'xhigh',
      ultra: 'ultra'
    })[choice?.thinkingEffort] || null;
  }

  function readPickerSnapshot(node) {
    let fiber = node && fiberOf(node);
    for (let up = 0; fiber && up < MAX_CLIMB; up += 1, fiber = fiber.return) {
      const owner = fiber.memoizedProps;
      const props = owner?.composerIntelligencePickerState ? owner : owner?.dropdownContent?.props;
      const state = props?.composerIntelligencePickerState;
      const data = props?.modelsData;
      if (!state || !Array.isArray(data?.versions)) continue;
      if (
        data.versions.length > 20 ||
        !Array.isArray(state.bucketSelections) ||
        state.bucketSelections.length > 12
      ) {
        return null;
      }

      const choices = state.bucketSelections.map((choice) => {
        const shortName = label(choice.category?.shortLabel);
        const familyId = groupId(choice.category?.modelVersion) || strictId(choice.modelSlug);
        const family = data.versions.find((version) => version.id === familyId);
        return {
          bucket: choice.bucket,
          id: strictId(choice.modelSlug),
          label: shortName && /^\d/.test(shortName) ? `GPT-${shortName}` : shortName,
          effort: effortOf(choice),
          familyId,
          familyLabel:
            label(family?.displayTextForIntelligence) ||
            label(choice.modelConfig?.title) ||
            (shortName && /^\d/.test(shortName) ? `GPT-${shortName}` : shortName),
          available:
            choice.availability?.status === 'available' &&
            !props.modelSwitcherDenialsBySlug?.[choice.modelSlug]
        };
      });

      const versions = data.versions
        .filter((version) => version.enabled === true)
        .map((version) => ({
          id: groupId(version.id),
          label: label(version.displayTextForIntelligence)
        }));

      if (
        !versions.length ||
        versions.some((version) => !version.id || !version.label) ||
        choices.some(
          (choice) =>
            !Number.isInteger(choice.bucket) ||
            !choice.id ||
            !choice.label ||
            !choice.effort ||
            !choice.familyId ||
            !choice.familyLabel
        ) ||
        new Set(versions.map((version) => version.id)).size !== versions.length ||
        new Set(choices.map((choice) => choice.bucket)).size !== choices.length
      ) {
        return null;
      }

      const version = groupId(state.selectedVersionEntry?.id);
      const currentBucket = state.currentBucket;
      if (
        !versions.some((entry) => entry.id === version) ||
        !choices.some((choice) => choice.bucket === currentBucket)
      ) {
        return null;
      }

      const selected = state.currentSelection;
      const chosen = choices.find((choice) => choice.bucket === currentBucket);
      if (selected?.modelSlug !== chosen.id || effortOf(selected) !== chosen.effort) return null;
      return { version, currentBucket, versions, choices };
    }
    return null;
  }

  function shellProExecutionModel(value) {
    const normalized = typeof value === 'string' ? value.trim().toLowerCase() : '';
    return /^(?:pro|(?:gpt-?)?\d+(?:[.-]\d+)?-pro)$/.test(normalized);
  }

  function closedPickerSelection(node) {
    const machine = node?.getAttribute?.('data-selected-reasoning-effort');
    const captionEffort = ({
      instant: 'none',
      minimal: 'minimal',
      low: 'low',
      medium: 'medium',
      high: 'high',
      'extra high': 'xhigh',
      max: 'max',
      ultra: 'ultra',
      pro: 'pro'
    })[String(node?.textContent || '').trim().toLowerCase()];
    let model = null;
    let lane = null;
    for (let fiber = fiberOf(node), up = 0; fiber && up < MAX_CLIMB; up += 1, fiber = fiber.return) {
      const selected = fiber.memoizedProps?.selectedPowerSelection ?? fiber.memoizedProps?.selectedLabelCandidate;
      if (lane === null && selected) {
        lane = {
          model: selected.model,
          effort: ({
            instant: 'none', minimal: 'minimal', low: 'low', medium: 'medium', high: 'high',
            'extra high': 'xhigh', max: 'max', ultra: 'ultra', pro: 'pro'
          })[String(selected.labels?.effort ?? selected.sliderLabel ?? '').trim().toLowerCase()] || null
        };
      }
      const current = fiber.memoizedProps?.currentModelId;
      if (current === undefined) continue;
      if (
        typeof current !== 'string' ||
        !/^[a-zA-Z0-9._-]{1,80}$/.test(current) ||
        (model && model !== current)
      ) return null;
      model = current;
    }
    const effort = shellProExecutionModel(model)
      ? 'pro'
      : (lane && lane.model === model && lane.effort) ||
        (machine !== null
          ? (['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra', 'pro'].includes(machine) ? machine : null)
          : captionEffort);
    return model && effort ? { id: model, effort } : null;
  }

  function readShellPickerSnapshot(node) {
    for (let fiber = node && fiberOf(node), up = 0; fiber && up < MAX_CLIMB; up += 1, fiber = fiber.return) {
      const props = fiber.memoizedProps;
      if (!Array.isArray(props?.powerSelections)) continue;
      const selected = props.selectedPowerSelection ?? props.selectedLabelCandidate;
      const options = props.modelListConfig?.options;
      if (!selected || !Array.isArray(options) || options.length > 20 || props.powerSelections.length > 12) {
        return null;
      }
      const effort = (value) => ({
        none: 'none', instant: 'none', minimal: 'minimal', min: 'low', low: 'low', standard: 'medium',
        medium: 'medium', extended: 'high', high: 'high', xhigh: 'xhigh', 'extra high': 'xhigh',
        max: 'max', ultra: 'ultra', pro: 'pro'
      })[value] || null;
      const laneEffort = (choice) => shellProExecutionModel(choice?.model)
        ? 'pro'
        : effort(String(choice?.labels?.effort ?? choice?.sliderLabel ?? '').trim().toLowerCase()) ||
          effort(choice?.reasoningEffort);
      const current = options.filter((option) => option?.selected === true);
      if (current.length !== 1) return null;
      const version = groupId(current[0].id);
      const versions = options
        .filter((option) => option && option.disabled !== true)
        .map((option) => ({ id: groupId(option.id), label: label(option.label) }));
      const choices = props.powerSelections.map((choice) => ({
        bucket: choice?.powerSettingIndex,
        id: strictId(choice?.model),
        label: label(choice?.modelLabel),
        familyId: strictId(choice?.model),
        familyLabel: label(choice?.modelLabel),
        effort: laneEffort(choice),
        available:
          props.modelSelectionDisabled !== true &&
          choice?.disabled !== true &&
          (!choice?.availability || choice.availability.status === 'available') &&
          !props.modelSwitcherDenialsBySlug?.[choice?.model]
      }));
      if (
        !version ||
        !versions.length ||
        versions.some((entry) => !entry.id || !entry.label) ||
        !choices.length ||
        choices.some((choice) =>
          !Number.isInteger(choice.bucket) || !choice.id || !choice.label || !choice.familyId || !choice.familyLabel || !choice.effort
        ) ||
        new Set(versions.map((entry) => entry.id)).size !== versions.length ||
        new Set(choices.map((choice) => choice.bucket)).size !== choices.length ||
        !versions.some((entry) => entry.id === version)
      ) return null;
      const selectedId = strictId(selected.model);
      const selectedEffort = laneEffort(selected);
      const matches = choices.filter((choice) => choice.id === selectedId && choice.effort === selectedEffort);
      if (
        matches.length !== 1 ||
        (selected.powerSettingIndex !== undefined && selected.powerSettingIndex !== matches[0].bucket)
      ) return null;
      return { version, currentBucket: matches[0].bucket, versions, choices };
    }
    return null;
  }

  function pickerSnapshot() {
    const triggers = modelPickerTriggers();
    const candidates = [];
    for (const trigger of triggers) {
      try {
        const picker = readPickerSnapshot(trigger) || readShellPickerSnapshot(trigger);
        if (picker) candidates.push({ trigger, picker });
      } catch {
        // An unrelated native menu is not picker evidence.
      }
    }
    const explicit = candidates.filter(({ trigger }) => trigger.matches?.(PICKER_TRIGGER));
    const identified = explicit.length === 1
      ? explicit[0]
      : candidates.length === 1
        ? candidates[0]
        : null;
    const native = triggers.filter((trigger) => trigger.matches?.(PICKER_TRIGGER));
    const fallback = native.length === 1
      ? native[0]
      : triggers.length === 1
        ? triggers[0]
        : null;
    const panel = document.querySelector(
      '[data-testid="composer-intelligence-picker-content"], [data-model-picker-view]'
    );
    const node = panel || identified?.trigger || fallback;

    let state = identified?.picker || null;
    if (!state && panel) {
      try {
        state = readPickerSnapshot(panel) || readShellPickerSnapshot(panel);
      } catch {
        state = null;
      }
    }
    const selected = state
      ? state.choices.find((choice) => choice.bucket === state.currentBucket && choice.available) || null
      : (fallback && node === fallback ? closedPickerSelection(fallback) : null);
    const provenTrigger = identified?.trigger || (selected && fallback ? fallback : null);

    // Stamp provider-proven ownership onto one exact native trigger. Open shell portals often own
    // no picker Fiber themselves, so the trigger remains the authority even while a menu is open.
    for (const trigger of triggers) {
      if (trigger === provenTrigger) trigger.setAttribute?.('data-moondesk-picker-route', location.pathname);
      else trigger.removeAttribute?.('data-moondesk-picker-route');
    }
    for (const [attribute, value] of [
      ['data-moondesk-selected-model', selected?.id],
      ['data-moondesk-selected-effort', selected?.effort],
      ['data-moondesk-selected-route', selected && location.pathname]
    ]) {
      if (!node) continue;
      if (!value) node.removeAttribute?.(attribute);
      else node.setAttribute?.(attribute, value);
    }

    if (state) return state;
    return selected ? { selected } : null;
  }

  window.addEventListener('message', (event) => {
    if (event.source !== window || event.origin !== location.origin) return;
    const data = event.data;
    if (!data || data.v !== 1) return;
    const nonce = typeof data.nonce === 'string' ? data.nonce.slice(0, 64) : '';
    if (!nonce) return;

    const previousPaths = currentPaths;
    currentPaths = new WeakMap();
    try {
      if (data.source === ASK) {
        let picker = null;
        try {
          picker = pickerSnapshot();
        } catch {
          picker = null;
        }
        window.postMessage({ source: REPLY, nonce, v: 1, picker }, location.origin);
        return;
      }

      if (data.source === CORRELATION_ASK) {
        let correlation = null;
        try {
          correlation = correlationSnapshot();
        } catch {
          correlation = null;
        }
        window.postMessage(
          { source: CORRELATION_REPLY, nonce, v: 1, correlation },
          location.origin
        );
      }
    } finally {
      currentPaths = previousPaths;
    }
  });
})();
