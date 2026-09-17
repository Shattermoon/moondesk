(() => {
  if (window.__MOONDESK_WORKER_MODEL_STATE_MAIN__) return;
  window.__MOONDESK_WORKER_MODEL_STATE_MAIN__ = true;

  const ASK = 'moondesk-picker-ask';
  const REPLY = 'moondesk-picker-reply';
  const MAX_CLIMB = 80;

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
    if (!data || data.source !== ASK || data.v !== 1) return;
    const nonce = typeof data.nonce === 'string' ? data.nonce.slice(0, 64) : '';
    if (!nonce) return;
    let picker = null;
    try {
      picker = pickerSnapshot();
    } catch {
      picker = null;
    }
    window.postMessage({ source: REPLY, nonce, v: 1, picker }, location.origin);
  });
})();
