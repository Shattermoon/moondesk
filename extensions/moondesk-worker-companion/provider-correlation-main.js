(() => {
  const OBSERVER_VERSION = 3;
  const prior = window.__MOONDESK_PROVIDER_CORRELATION_MAIN__;
  if (
    prior?.version === OBSERVER_VERSION &&
    typeof prior.refresh === 'function' &&
    prior.refresh() === true
  ) return;
  try { prior?.dispose?.(); } catch {}

  let active = true;
  const OBSERVED = 'moondesk-provider-correlation-observed';
  const ASK = 'moondesk-provider-correlation-ask';
  const REPLY = 'moondesk-provider-correlation-reply';
  const MAX_EVENT_BYTES = 512 * 1024;
  const MAX_RESPONSE_BYTES = 4 * 1024 * 1024;
  const MAX_WEBSOCKET_BYTES = 2 * 1024 * 1024;
  const MAX_BUFFERED_PROOFS = 32;
  const MAX_REQUEST_IDS = 16;
  const MAX_OPERATION_IDS = 8;
  const MAX_SCAN_NODES = 1024;
  const MAX_SCAN_DEPTH = 10;
  const PROOF_TTL_MS = 2 * 60 * 1000;
  const TOPIC_TTL_MS = 2 * 60 * 1000;
  const MESSAGE_TTL_MS = 2 * 60 * 1000;
  const MAX_REQUEST_BODY_BYTES = 512 * 1024;

  const proofs = [];
  const proofKeys = new Map();
  const topicRoutes = new Map();
  const messageRoutes = new Map();
  const inspectedResponses = new WeakSet();
  const observedSockets = new WeakSet();
  const originReaders = new Set();
  let observedFetch = null;
  let observedWebSocket = null;

  function normalizedRequestId(value) {
    if (typeof value !== 'string' || value.length > 160) return null;
    const id = value.split('/')[0].trim();
    return /^[a-z0-9_-]{1,100}$/i.test(id) ? id : null;
  }

  function normalizedOperationId(value) {
    if (typeof value !== 'string') return null;
    const id = value.trim().toLowerCase();
    return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(id) ? id : null;
  }

  function normalizedConversationId(value) {
    if (typeof value !== 'string') return null;
    const id = value.trim().toLowerCase();
    return /^[0-9a-f-]{16,64}$/i.test(id) ? id : null;
  }

  function normalizedTopicId(value) {
    if (typeof value !== 'string' || value.length > 256) return null;
    const topic = value.trim();
    return /^conversation-turn-[a-z0-9_-]{4,220}$/i.test(topic) ? topic : null;
  }

  function normalizedMessageId(value) {
    if (typeof value !== 'string' || value.length > 160) return null;
    const id = value.trim();
    return /^[a-z0-9_-]{8,128}$/i.test(id) ? id : null;
  }

  function forgetProof(proof) {
    if (!proof) return;
    for (const requestId of proof.requestIds || []) {
      const key = `request\u0000${proof.conversationId}\u0000${requestId}`;
      if (proofKeys.get(key) === proof.observedAt) proofKeys.delete(key);
    }
    for (const operationId of proof.operationIds || []) {
      const key = `operation\u0000${proof.conversationId}\u0000${operationId}`;
      if (proofKeys.get(key) === proof.observedAt) proofKeys.delete(key);
    }
  }

  function prune(now = Date.now()) {
    while (proofs.length && now - proofs[0].observedAt > PROOF_TTL_MS) {
      forgetProof(proofs.shift());
    }
    for (const [topic, route] of topicRoutes) {
      if (now - route.observedAt > TOPIC_TTL_MS) topicRoutes.delete(topic);
    }
    for (const [messageId, route] of messageRoutes) {
      if (now - route.observedAt > MESSAGE_TTL_MS) messageRoutes.delete(messageId);
    }
  }

  function rememberTopic(topicId, conversationId) {
    const topic = normalizedTopicId(topicId);
    const conversation = normalizedConversationId(conversationId);
    if (!topic || !conversation) return;
    prune();
    const existing = topicRoutes.get(topic);
    if (existing && existing.conversationId !== conversation) {
      topicRoutes.delete(topic);
      return;
    }
    topicRoutes.set(topic, { conversationId: conversation, observedAt: Date.now() });
  }

  function rememberMessage(messageId, conversationId) {
    const message = normalizedMessageId(messageId);
    const conversation = normalizedConversationId(conversationId);
    if (!message || !conversation) return;
    prune();
    const existing = messageRoutes.get(message);
    if (existing && existing.conversationId !== conversation) {
      messageRoutes.delete(message);
      return;
    }
    messageRoutes.set(message, { conversationId: conversation, observedAt: Date.now() });
  }

  function requestIdentityFor(url, init) {
    let parsed;
    try { parsed = new URL(String(url || ''), location.href); } catch { return null; }
    if (parsed.origin !== location.origin || parsed.pathname !== '/backend-api/f/conversation') return null;
    const body = init?.body;
    if (typeof body !== 'string' || body.length > MAX_REQUEST_BODY_BYTES) return null;
    const value = parseJson(body);
    if (!value || typeof value !== 'object') return null;
    const conversationId = normalizedConversationId(value.conversation_id || value.conversationId);
    const messageIds = [];
    const add = (candidate) => {
      const messageId = normalizedMessageId(candidate);
      if (messageId && !messageIds.includes(messageId)) messageIds.push(messageId);
    };
    add(value.parent_message_id || value.parentMessageId);
    if (Array.isArray(value.messages)) {
      for (const message of value.messages.slice(0, 8)) add(message?.id);
    }
    return { conversationId, messageIds: messageIds.slice(0, 16) };
  }

  // Worker spawn arguments can be present in ChatGPT's provider/page model before an MCP
  // transport header reaches MoonDesk. Project only our opaque idempotency UUID; never copy or
  // parse any other tool argument, prompt text, or result across the MAIN-world boundary.
  function collectOperationIdsFromText(value, operationIds) {
    if (
      typeof value !== 'string' || value.length > MAX_EVENT_BYTES ||
      (!value.includes('operation_id') && !value.includes('operationId'))
    ) return;
    const pattern = /["']?(?:operation_id|operationId)["']?\s*[:=]\s*["']([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})["']/ig;
    for (let match; operationIds.size < MAX_OPERATION_IDS && (match = pattern.exec(value));) {
      const operationId = normalizedOperationId(match[1]);
      if (operationId) operationIds.add(operationId);
    }
  }

  function emitProof(conversationId, requestIds = [], operationIds = []) {
    const conversation = normalizedConversationId(conversationId);
    if (!conversation) return;
    const requests = [...new Set(requestIds.map(normalizedRequestId).filter(Boolean))].slice(0, MAX_REQUEST_IDS);
    const operations = [...new Set(operationIds.map(normalizedOperationId).filter(Boolean))].slice(0, MAX_OPERATION_IDS);
    if (!requests.length && !operations.length) return;
    const now = Date.now();
    prune(now);
    const freshRequests = requests.filter((requestId) =>
      !proofKeys.has(`request\u0000${conversation}\u0000${requestId}`));
    const freshOperations = operations.filter((operationId) =>
      !proofKeys.has(`operation\u0000${conversation}\u0000${operationId}`));
    if (!freshRequests.length && !freshOperations.length) return;
    const proof = {
      conversationId: conversation,
      requestIds: freshRequests,
      operationIds: freshOperations,
      observedAt: now
    };
    proofs.push(proof);
    while (proofs.length > MAX_BUFFERED_PROOFS) forgetProof(proofs.shift());
    for (const requestId of freshRequests) {
      proofKeys.set(`request\u0000${conversation}\u0000${requestId}`, now);
    }
    for (const operationId of freshOperations) {
      proofKeys.set(`operation\u0000${conversation}\u0000${operationId}`, now);
    }
    window.postMessage({ source: OBSERVED, v: 1, correlation: {
      conversationId: conversation,
      ...(freshRequests.length ? { requestIds: freshRequests } : {}),
      ...(freshOperations.length ? { operationIds: freshOperations } : {})
    } }, location.origin);
  }

  function collectIdentifiers(value) {
    const conversationIds = new Set();
    const requestIds = new Set();
    const operationIds = new Set();
    const topicIds = new Set();
    const messageIds = new Set();
    const encodedItems = [];
    const stack = [{ value, depth: 0 }];
    let scanned = 0;

    while (stack.length && scanned < MAX_SCAN_NODES) {
      const item = stack.pop();
      const current = item.value;
      const depth = item.depth;
      scanned += 1;
      if (!current || depth > MAX_SCAN_DEPTH) continue;
      if (typeof current === 'string') {
        collectOperationIdsFromText(current, operationIds);
        continue;
      }
      if (Array.isArray(current)) {
        for (let index = current.length - 1; index >= 0; index -= 1) {
          stack.push({ value: current[index], depth: depth + 1 });
        }
        continue;
      }
      if (typeof current !== 'object') continue;

      for (const [key, child] of Object.entries(current)) {
        if (typeof child === 'string') collectOperationIdsFromText(child, operationIds);
        if (key === 'conversation_id' || key === 'conversationId') {
          const conversation = normalizedConversationId(child);
          if (conversation) conversationIds.add(conversation);
        } else if (key === 'request_id' || key === 'requestId') {
          const requestId = normalizedRequestId(child);
          if (requestId) requestIds.add(requestId);
        } else if (key === 'operation_id' || key === 'operationId') {
          const operationId = normalizedOperationId(child);
          if (operationId) operationIds.add(operationId);
        } else if (key === 'topic_id' || key === 'topicId' || key === 'topic') {
          const topic = normalizedTopicId(child);
          if (topic) topicIds.add(topic);
        } else if (
          key === 'message_id' || key === 'messageId' ||
          key === 'parent_message_id' || key === 'parentMessageId' ||
          key === 'current_message_id' || key === 'currentMessageId'
        ) {
          const messageId = normalizedMessageId(child);
          if (messageId) messageIds.add(messageId);
        } else if (key === 'encoded_item' && typeof child === 'string' && child.length <= MAX_EVENT_BYTES) {
          encodedItems.push(child);
          continue;
        }
        if (child && typeof child === 'object') stack.push({ value: child, depth: depth + 1 });
      }
    }
    return { conversationIds, requestIds, operationIds, topicIds, messageIds, encodedItems };
  }

  function parseJson(value) {
    if (typeof value !== 'string' || !value) return null;
    try { return JSON.parse(value); } catch { return null; }
  }

  function analyzeValue(value, inheritedConversationId = null) {
    const identifiers = collectIdentifiers(value);
    let conversationId = null;
    if (identifiers.conversationIds.size === 1) {
      conversationId = [...identifiers.conversationIds][0];
    } else if (identifiers.conversationIds.size === 0) {
      const inherited = normalizedConversationId(inheritedConversationId);
      const routed = [
        ...[...identifiers.topicIds].map((topic) => topicRoutes.get(topic)?.conversationId || null),
        ...[...identifiers.messageIds].map((messageId) => messageRoutes.get(messageId)?.conversationId || null)
      ].filter(Boolean);
      const routedSet = new Set(routed);
      if (routedSet.size === 1) conversationId = [...routedSet][0];
      else if (routedSet.size === 0) conversationId = inherited;
    }

    if (conversationId) {
      for (const topic of identifiers.topicIds) rememberTopic(topic, conversationId);
      for (const messageId of identifiers.messageIds) rememberMessage(messageId, conversationId);
      if (identifiers.requestIds.size || identifiers.operationIds.size) {
        emitProof(
          conversationId,
          [...identifiers.requestIds],
          [...identifiers.operationIds]
        );
      }
    }

    for (const encoded of identifiers.encodedItems) {
      consumeSseText(encoded, conversationId || inheritedConversationId);
    }
    return conversationId;
  }

  function consumeSseEvent(eventText, inheritedConversationId = null) {
    const data = String(eventText || '')
      .split(/\r?\n/)
      .filter((line) => line.startsWith('data:'))
      .map((line) => line.slice(5).trimStart())
      .join('\n')
      .trim();
    if (!data || data === '[DONE]') return inheritedConversationId;
    const parsed = parseJson(data);
    if (parsed === null) return inheritedConversationId;
    return analyzeValue(parsed, inheritedConversationId) || inheritedConversationId;
  }

  function consumeSseText(text, inheritedConversationId = null) {
    if (typeof text !== 'string' || !text || text.length > MAX_EVENT_BYTES) return inheritedConversationId;
    let conversationId = inheritedConversationId;
    const events = text.split(/\r?\n\r?\n/);
    for (const event of events) {
      conversationId = consumeSseEvent(event, conversationId) || conversationId;
    }
    return conversationId;
  }

  function consumeProviderText(text, inheritedConversationId = null) {
    if (typeof text !== 'string' || !text || text.length > MAX_EVENT_BYTES) return inheritedConversationId;
    const trimmed = text.trim();
    if (!trimmed) return inheritedConversationId;
    if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
      const parsed = parseJson(trimmed);
      return parsed === null
        ? inheritedConversationId
        : analyzeValue(parsed, inheritedConversationId) || inheritedConversationId;
    }
    return consumeSseText(text, inheritedConversationId);
  }

  async function inspectEventStream(response, requestIdentity = null) {
    if (!active || !response || typeof response !== 'object' || inspectedResponses.has(response)) return;
    inspectedResponses.add(response);
    let clone;
    try { clone = response.clone(); } catch { return; }
    const body = clone?.body;
    if (!body || typeof body.getReader !== 'function' || typeof TextDecoder !== 'function') return;
    if (originReaders.size >= 2) return;
    const reader = body.getReader();
    originReaders.add(reader);
    const decoder = new TextDecoder();
    let bytes = 0;
    let buffer = '';
    let conversationId = normalizedConversationId(requestIdentity?.conversationId);
    const requestMessageIds = Array.isArray(requestIdentity?.messageIds)
      ? requestIdentity.messageIds.map(normalizedMessageId).filter(Boolean).slice(0, 16)
      : [];
    const bindRequestMessages = () => {
      if (!conversationId) return;
      for (const messageId of requestMessageIds) rememberMessage(messageId, conversationId);
    };
    bindRequestMessages();
    try {
      while (bytes < MAX_RESPONSE_BYTES) {
        const { done, value } = await reader.read();
        if (done) break;
        if (!(value instanceof Uint8Array)) break;
        bytes += value.byteLength;
        if (bytes > MAX_RESPONSE_BYTES) break;
        buffer += decoder.decode(value, { stream: true });
        let boundary;
        while ((boundary = buffer.search(/\r?\n\r?\n/)) >= 0) {
          const match = /\r?\n\r?\n/.exec(buffer.slice(boundary));
          const separatorLength = match?.[0]?.length || 2;
          const event = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + separatorLength);
          conversationId = consumeSseEvent(event, conversationId) || conversationId;
          bindRequestMessages();
        }
        if (buffer.length > MAX_EVENT_BYTES) break;
      }
      buffer += decoder.decode();
      if (buffer.trim()) {
        conversationId = consumeProviderText(buffer, conversationId) || conversationId;
        bindRequestMessages();
      }
    } catch {
      // Provider transport inspection must never affect the ChatGPT request.
    } finally {
      originReaders.delete(reader);
      try { await reader.cancel(); } catch {}
    }
  }

  function eligibleFetchResponse(url, response) {
    let parsed;
    try { parsed = new URL(String(url || ''), location.href); } catch { return false; }
    if (parsed.origin !== location.origin || !parsed.pathname.startsWith('/backend-api/')) return false;
    if (parsed.pathname === '/backend-api/f/conversation') return true;
    prune();
    if (!topicRoutes.size && !messageRoutes.size) return false;
    const contentType = response?.headers?.get?.('content-type') || '';
    return /text\/event-stream/i.test(contentType);
  }

  function installFetchObserver() {
    if (!active || typeof window.fetch !== 'function' || window.fetch === observedFetch) return;
    const downstreamFetch = window.fetch;
    const nextObservedFetch = function (input, init) {
      let url = null;
      try { url = typeof input === 'string' || input instanceof URL ? String(input) : input?.url || null; } catch {}
      const requestIdentity = url ? requestIdentityFor(url, init) : null;
      const result = downstreamFetch.apply(this, arguments);
      if (url && result && typeof result.then === 'function') {
        result.then((response) => {
          if (eligibleFetchResponse(url, response)) void inspectEventStream(response, requestIdentity);
        }).catch(() => {});
      }
      return result;
    };
    try { Object.setPrototypeOf(nextObservedFetch, downstreamFetch); } catch {}
    observedFetch = nextObservedFetch;
    window.fetch = observedFetch;
  }

  function inspectWebSocketData(data) {
    if (typeof data !== 'string' || !data || data.length > MAX_WEBSOCKET_BYTES) return;
    const parsed = parseJson(data);
    if (parsed !== null) {
      analyzeValue(parsed, null);
      return;
    }
    consumeSseText(data, null);
  }

  function installWebSocketObserver() {
    if (!active || typeof window.WebSocket !== 'function' || window.WebSocket === observedWebSocket) return;
    const DownstreamWebSocket = window.WebSocket;
    function ObservedWebSocket(url, protocols) {
      const socket = protocols === undefined
        ? new DownstreamWebSocket(url)
        : new DownstreamWebSocket(url, protocols);
      try {
        if (!observedSockets.has(socket)) {
          observedSockets.add(socket);
          socket.addEventListener('message', (event) => inspectWebSocketData(event?.data));
        }
      } catch {}
      return socket;
    }
    try { Object.setPrototypeOf(ObservedWebSocket, DownstreamWebSocket); } catch {}
    ObservedWebSocket.prototype = DownstreamWebSocket.prototype;
    observedWebSocket = ObservedWebSocket;
    window.WebSocket = observedWebSocket;
  }

  window.addEventListener('message', (event) => {
    if (event.source !== window || event.origin !== location.origin) return;
    const data = event.data;
    if (!data || data.v !== 1 || data.source !== ASK || typeof data.nonce !== 'string') return;
    prune();
    window.postMessage({
      source: REPLY,
      nonce: data.nonce.slice(0, 64),
      v: 1,
      correlations: proofs.map((proof) => ({
        conversationId: proof.conversationId,
        ...((proof.requestIds || []).length ? { requestIds: [...proof.requestIds] } : {}),
        ...((proof.operationIds || []).length ? { operationIds: [...proof.operationIds] } : {})
      }))
    }, location.origin);
  });

  function refreshObservers() {
    if (!active) return false;
    installFetchObserver();
    installWebSocketObserver();
    return true;
  }

  refreshObservers();
  if (typeof document !== 'undefined' && document.readyState === 'loading') {
    window.addEventListener('DOMContentLoaded', refreshObservers, { once: true });
  }
  window.__MOONDESK_PROVIDER_CORRELATION_MAIN__ = {
    version: OBSERVER_VERSION,
    refresh: refreshObservers,
    dispose() {
      active = false;
      for (const reader of originReaders) void reader.cancel().catch(() => {});
      originReaders.clear();
    }
  };
})();
