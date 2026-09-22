(() => {
  if (window.__MOONDESK_WORKER_MODEL_STATE_MAIN__) return;
  window.__MOONDESK_WORKER_MODEL_STATE_MAIN__ = true;

  const ASK = 'moondesk-picker-ask';
  const REPLY = 'moondesk-picker-reply';
  const CORRELATION_ASK = 'moondesk-correlation-ask';
  const CORRELATION_REPLY = 'moondesk-correlation-reply';
  const MAX_CLIMB = 80;
  const MAX_CORRELATION_TURNS = 16;
  const MAX_CORRELATION_IDS = 32;

  function fiberOf(node) {
    if (!node) return null;
    for (const key in node) {
      if (key.startsWith('__reactFiber$')) return node[key];
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
      sections = [...document.querySelectorAll('section[data-testid^="conversation-turn"]')];
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

  function modelPickerTrigger() {
    const form = document.querySelector('#prompt-textarea')?.closest('form');
    const triggers = [...(form?.querySelectorAll('button[aria-haspopup="menu"]') || [])].filter(
      (node) =>
        visible(node) &&
        node.id !== 'composer-plus-btn' &&
        node.getAttribute('data-testid') !== 'composer-plus-btn'
    );
    return triggers.length === 1 ? triggers[0] : null;
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

  function closedPickerSelection(node) {
    const effort = ({
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
    if (!effort) return null;

    let model = null;
    for (let fiber = fiberOf(node), up = 0; fiber && up < MAX_CLIMB; up += 1, fiber = fiber.return) {
      const current = fiber.memoizedProps?.currentModelId;
      if (current === undefined) continue;
      if (
        typeof current !== 'string' ||
        !/^[a-zA-Z0-9._-]{1,80}$/.test(current) ||
        (model && model !== current)
      ) {
        return null;
      }
      model = current;
    }
    return model ? { id: model, effort } : null;
  }

  function pickerSnapshot() {
    const trigger = modelPickerTrigger();
    const node =
      document.querySelector('[data-testid="composer-intelligence-picker-content"]') || trigger;
    let state = null;
    try {
      state = readPickerSnapshot(node);
    } catch {
      state = null;
    }
    if (state) return state;

    // Closed-picker state is enough only for exact current-selection readback, not catalog discovery.
    const selected = trigger && node === trigger ? closedPickerSelection(trigger) : null;
    return selected ? { selected } : null;
  }

  window.addEventListener('message', (event) => {
    if (event.source !== window || event.origin !== location.origin) return;
    const data = event.data;
    if (!data || data.v !== 1) return;
    const nonce = typeof data.nonce === 'string' ? data.nonce.slice(0, 64) : '';
    if (!nonce) return;

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
  });
})();
