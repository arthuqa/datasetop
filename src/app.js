import { renderMarkdown } from './markdown.js';

// datasetop — chat-only main window frontend.
//
// The whole UI is a single conversation: choose a folder, run or stop the
// agent, tweak OpenAI settings, and talk to the agent. Chat lines arrive live
// over the `agent-chat` / `agent-message` events and are replayed on boot from
// `get_chat_history`.
const isBrowser = typeof window !== 'undefined' && typeof document !== 'undefined';
const T = (isBrowser && window.__TAURI__) || {};
const invoke = T.core?.invoke;
const listen = T.event?.listen;
const appWindow = T.window?.getCurrentWindow?.() || T.webviewWindow?.getCurrentWebviewWindow?.();
// Avoid painting Windows controls on macOS before the first backend snapshot.
if (isBrowser) {
  const earlyPlatform = navigator.platform || '';
  document.documentElement.classList.add(earlyPlatform.startsWith('Mac') ? 'platform-macos' : earlyPlatform.startsWith('Win') ? 'platform-windows' : 'platform-linux');
}

const $ = id => document.getElementById(id);

// --- state ------------------------------------------------------------------
let snapshot = null;
let chatItems = [];
let chatFollow = true;
let historyVersion = 0;
let mcpConfigLoaded = false;
let mcpLoading = false;
let mcpDirty = false;
let settingsSaving = false;
let settingsEpoch = 0;
let toastTimer = null;
let clearTimer = null;
let streamTimer = null;
let streamBuffer = '';
// Bumped whenever the log is reset so in-flight image loads can detect that
// they belong to a conversation that no longer exists.
let chatEpoch = 0;
let chatDetailId = 0;
let toolCatalogEpoch = 0;
const CHAT_ITEMS_MAX = 250;
const MAX_MCP_SERVERS = 32;
const MCP_SERVER_NAME = /^[a-zA-Z0-9_]{1,32}$/;

// --- MCP server model -------------------------------------------------------
// A connection is either a full URL or an executable. Arguments remain an
// array, never a shell command line. No runtime or package is special-cased.
const WORKSPACE_TOKEN = '${workspaceFolder}';
const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;

// One argument per line keeps values that contain spaces intact and avoids any
// shell-style quoting or splitting.
function parseArgumentLines(text) {
  return String(text ?? '')
    .split(/\r?\n/)
    .map(line => line.trim())
    .filter(line => line.length > 0);
}

function argumentsToText(args) {
  return (Array.isArray(args) ? args : []).join('\n');
}

// `KEY=value` per line; blank lines and `#` comments are ignored. Only the key
// is validated, and a value may itself contain `=`.
function parseEnvLines(text) {
  const env = {};
  const invalid = [];
  for (const raw of String(text ?? '').split(/\r?\n/)) {
    const line = raw.trim().replace(/^export\s+/, '');
    if (!line || line.startsWith('#')) continue;
    const index = line.indexOf('=');
    const key = (index === -1 ? line : line.slice(0, index)).trim();
    const value = index === -1 ? '' : line.slice(index + 1).trim();
    if (index === -1 || !ENV_KEY.test(key)) { invalid.push(line); continue; }
    env[key] = value;
  }
  return { env, invalid };
}

function envToText(env) {
  return Object.entries(env && typeof env === 'object' ? env : {})
    .map(([key, value]) => `${key}=${value}`)
    .join('\n');
}

function isRemoteConnection(connection) {
  const value = String(connection ?? '').trim();
  return /^[a-z][a-z\d+.-]*:\/\//i.test(value) || /^https?:/i.test(value);
}

// The backend migrates legacy package entries before returning them to the UI.
function normalizeServer(server = {}) {
  const args = Array.isArray(server.args) ? server.args.map(String) : [];
  const env = server.env && typeof server.env === 'object' && !Array.isArray(server.env)
    ? Object.fromEntries(Object.entries(server.env).map(([key, value]) => [String(key), String(value)]))
    : {};
  return {
    name: typeof server.name === 'string' ? server.name : '',
    enabled: server.enabled !== false,
    connection: String(server.url || server.command || '').trim(),
    args,
    env,
  };
}

// Turn the editable row model back into the stored shape.
function rowToServer(row = {}) {
  const enabled = row.enabled !== false;
  const args = Array.isArray(row.args) ? row.args.slice() : [];
  const env = row.env && typeof row.env === 'object' ? { ...row.env } : {};
  const name = String(row.name ?? '').trim();
  const connection = String(row.connection ?? '').trim();
  if (isRemoteConnection(connection)) {
    return { name, url: connection, args: [], env: {}, enabled };
  }
  return { name, url: '', command: connection, args, env, enabled };
}

function remoteUrlError(url) {
  const value = String(url ?? '').trim();
  if (!value) return 'enter the server URL.';
  if (!/^https?:\/\//i.test(value)) return 'use a full HTTPS URL (localhost HTTP is allowed).';
  let parsed;
  try { parsed = new URL(value); } catch { return 'enter a valid URL.'; }
  const local = parsed.hostname === 'localhost' || parsed.hostname === '127.0.0.1';
  if (parsed.protocol !== 'https:' && !(parsed.protocol === 'http:' && local)) return 'use an HTTPS URL (localhost HTTP is allowed).';
  if (parsed.username || parsed.password) return 'remove any username or password from the URL.';
  if (parsed.hash) return 'remove the #fragment from the URL.';
  return '';
}

// Returns `{ index, field, error }`; `index` is the offending row (or -1) and
// `field` names the control to focus.
function validateRows(rows, { max = MAX_MCP_SERVERS } = {}) {
  if (rows.length > max) return { index: 0, field: 'name', error: `At most ${max} MCP servers are supported.` };
  const names = new Set();
  for (let index = 0; index < rows.length; index += 1) {
    const row = rows[index];
    const name = String(row.name ?? '').trim();
    if (!MCP_SERVER_NAME.test(name)) return { index, field: 'name', error: 'Give every server a unique name (letters, numbers or underscores, up to 32 characters).' };
    if (names.has(name)) return { index, field: 'name', error: `“${name}” is already used by another server.` };
    names.add(name);
    const connection = String(row.connection ?? '').trim();
    if (!connection) return { index, field: 'connection', error: `${name}: enter a command or URL.` };
    if (isRemoteConnection(connection)) {
      const issue = remoteUrlError(connection);
      if (issue) return { index, field: 'connection', error: `${name}: ${issue}` };
    }
  }
  return { index: -1, field: '', error: '' };
}

// --- platform ---------------------------------------------------------------
// The backend reports the host platform in every snapshot; the title bar picks
// its control layout from the resulting root class.
function iconPlatform(platform = snapshot?.platform) {
  if (platform === 'windows') return 'windows';
  if (platform === 'macos') return 'macos';
  return 'ubuntu';
}

function applyPlatform(platform) {
  const root = document.documentElement;
  root.classList.remove('platform-macos', 'platform-windows', 'platform-linux');
  root.classList.add(`platform-${platform || 'linux'}`);
  const icon = $('folderIcon');
  if (icon) icon.src = `./icons/folder_${iconPlatform(platform)}.svg`;
}

// --- small helpers ----------------------------------------------------------
function showToast(message) {
  const el = $('toast');
  el.textContent = message;
  el.classList.remove('hidden');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.add('hidden'), 2400);
}

function errorText(err) {
  if (err == null) return 'Unknown error';
  return typeof err === 'string' ? err : (err.message || String(err));
}

// --- chat rendering ---------------------------------------------------------
// One scrollable log. User turns are bubbles, agent text streams in place,
// reasoning and tool calls are collapsible so long traces stay readable.
function chatLog() {
  return $('chatLog');
}

function chatScrollToBottom() {
  const log = chatLog();
  log.scrollTop = log.scrollHeight;
}

function chatFollowOutput() {
  if (chatFollow) chatScrollToBottom();
  $('jumpBottom').classList.toggle('hidden', chatFollow);
}

function chatTrim() {
  while (chatItems.length > CHAT_ITEMS_MAX) {
    const item = chatItems.shift();
    item.node?.remove();
  }
}

function chatAppendMessage({ role, kind = 'message', text = '', streaming = false }) {
  const node = document.createElement('div');
  node.className = `chat-item ${role} ${kind}${streaming ? ' streaming' : ''}`;
  const body = document.createElement('div');
  body.className = 'chat-text';
  if (role === 'agent' && kind === 'message') renderMarkdown(body, text);
  else body.textContent = text;
  node.appendChild(body);
  const item = { role, kind, node, body, text, streaming };
  chatItems.push(item);
  chatLog().appendChild(node);
  chatTrim();
  chatFollowOutput();
  return item;
}

function chatAppendCollapsible({ kind, title, args = '', text = '', collapsed = true, streaming = false }) {
  const node = document.createElement('div');
  node.className = `chat-item agent ${kind}${collapsed ? ' collapsed' : ''}${streaming ? ' streaming' : ''}`;
  const toggle = document.createElement('button');
  toggle.type = 'button';
  toggle.className = 'chat-toggle';
  toggle.setAttribute('aria-expanded', String(!collapsed));
  const caret = document.createElement('span');
  caret.className = 'chat-caret';
  caret.innerHTML = '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="m7 4 6 6-6 6"/></svg>';
  if (kind === 'tool') {
    const icon = document.createElement('span');
    icon.className = 'tool-icon';
    icon.innerHTML = '<svg viewBox="0 0 20 20" aria-hidden="true"><path d="m7 5-5 5 5 5m6-10 5 5-5 5M11.5 3l-3 14"/></svg>';
    toggle.appendChild(icon);
  }
  const heading = document.createElement('span');
  heading.className = 'chat-title';
  heading.textContent = title;
  toggle.append(caret, heading);
  let statusEl = null;
  if (kind === 'tool') {
    statusEl = document.createElement('span');
    statusEl.className = 'tool-status';
    statusEl.textContent = 'Running';
  }
  let argsEl = null;
  if (args) {
    argsEl = document.createElement('span');
    argsEl.className = 'chat-args';
    argsEl.textContent = args;
    toggle.appendChild(argsEl);
  }
  if (statusEl) toggle.appendChild(statusEl);
  const body = document.createElement('div');
  body.className = 'chat-body';
  body.id = `chat-detail-${++chatDetailId}`;
  toggle.setAttribute('aria-controls', body.id);
  const bodyText = document.createElement('span');
  bodyText.className = 'chat-text';
  bodyText.textContent = text;
  body.appendChild(bodyText);
  node.append(toggle, body);
  const item = { role: 'agent', kind, node, body: bodyText, bodyContainer: body, titleEl: heading, argsEl, statusEl, text, collapsed, streaming, userToggled: false, images: [], imagesSection: null };
  item.toggle = toggle;
  if (kind === 'tool') node.classList.add('tool-running');
  toggle.addEventListener('click', () => {
    chatSetCollapsed(item, !item.collapsed, true);
  });
  chatItems.push(item);
  chatLog().appendChild(node);
  chatTrim();
  chatFollowOutput();
  return item;
}

function chatSetCollapsed(item, collapsed, userToggled = false) {
  if (!item) return;
  item.collapsed = collapsed;
  if (userToggled) item.userToggled = true;
  item.node.classList.toggle('collapsed', collapsed);
  item.toggle?.setAttribute('aria-expanded', String(!collapsed));
  if (!collapsed) loadToolImages(item);
  chatFollowOutput();
}

function chatSetMessage(item, text) {
  item.text = text;
  if (item.role === 'agent' && item.kind === 'message') renderMarkdown(item.body, text);
  else item.body.textContent = text;
  chatFollowOutput();
}

// Reasoning usually opens with a bold title (`**Clarifying the request**`);
// that becomes the folded header so a collapsed block still reads as a summary.
function thinkingParts(text) {
  const match = text.match(/^\s*\*\*([^*\n]+)\*\*\s*/);
  if (match) return { title: match[1].trim(), body: text.slice(match[0].length) };
  return { title: 'thinking', body: text };
}

function chatSetThinking(item, text) {
  const parts = thinkingParts(text);
  item.text = text;
  item.titleEl.textContent = parts.title === 'thinking' ? (item.streaming ? 'Thinking…' : 'Thinking') : parts.title;
  item.body.textContent = parts.body;
  chatFollowOutput();
}

function chatStreamingMessage() {
  const last = chatItems[chatItems.length - 1];
  return last?.streaming && last.kind === 'message' ? last : null;
}

function flushStream() {
  clearTimeout(streamTimer);
  streamTimer = null;
  if (!streamBuffer) return;
  const item = chatStreamingMessage();
  if (item) chatSetMessage(item, item.text + streamBuffer);
  streamBuffer = '';
}

function chatFinishStreaming(text) {
  flushStream();
  const item = chatStreamingMessage();
  if (!item) return null;
  item.streaming = false;
  item.node.classList.remove('streaming');
  if (typeof text === 'string' && text.trim()) chatSetMessage(item, text);
  return item;
}

function chatCloseThinking() {
  const last = chatItems[chatItems.length - 1];
  if (last?.kind === 'thinking' && last.streaming) {
    last.streaming = false;
    last.node.classList.remove('streaming');
    if (!last.text.trim()) last.body.textContent = 'This model did not share its thoughts.';
    if (last.titleEl.textContent === 'Thinking…') last.titleEl.textContent = 'Thinking';
    if (!last.userToggled) chatSetCollapsed(last, true);
  }
}

function chatPendingTool(id, name) {
  for (let i = 0; i < chatItems.length; i += 1) {
    const item = chatItems[i];
    if (item.kind === 'tool' && item.result === undefined && (id ? item.callId === id : item.toolName === name)) return item;
  }
  return null;
}

function toolLabel(name = '') {
  const match = /^mcp__(.+?)__(.+)$/.exec(name);
  if (!match) return name.replaceAll('_', ' ');
  return `${match[1].replaceAll('_', ' ')} · ${match[2].replaceAll('_', ' ').replaceAll('-', ' ')}`;
}

function toolSummary(argumentsText) {
  try {
    const args = JSON.parse(argumentsText);
    const value = args.path || args.file_path || args.source || args.url || args.directory;
    if (typeof value === 'string') return value;
  } catch { /* A truncated preview may not be valid JSON. */ }
  return '';
}

function formattedToolValue(value) {
  if (typeof value !== 'string') return String(value ?? '');
  try {
    const parsed = JSON.parse(value);
    if (parsed && Array.isArray(parsed.content)) {
      const text = parsed.content.map(part => part.type === 'text'
        ? part.text
        : `[${part.type || 'media'}${part.mimeType ? ` · ${part.mimeType}` : ''} result]`).join('\n');
      if (text) return parsed.structuredContent
        ? `${text}\n\nStructured result\n${JSON.stringify(parsed.structuredContent, null, 2)}`
        : text;
    }
    return JSON.stringify(parsed, null, 2);
  } catch { return value; }
}

function formattedToolInput(value) {
  try { return JSON.stringify(JSON.parse(value), null, 2); } catch { return String(value ?? ''); }
}

// --- tool images ------------------------------------------------------------
// MCP tools can attach images. The backend persists only each saved path and
// mime type; the bytes arrive on demand from `read_tool_image` as a data URI.
// Records live on the chat item, so re-rendering the card reuses whatever was
// already loaded instead of fetching the same image twice.
function toolImageRecord(image = {}, index = 0) {
  const path = typeof image?.path === 'string' ? image.path : '';
  const mimeType = typeof image?.mime_type === 'string' ? image.mime_type : '';
  return { path, mimeType, caption: path || mimeType || `Image ${index + 1}`, state: 'idle', dataUri: '', error: '' };
}

function setToolImages(item, rawImages) {
  if (!item) return;
  const incoming = Array.isArray(rawImages) ? rawImages : [];
  const previous = item.images || [];
  item.images = incoming.map((image, index) => {
    const record = toolImageRecord(image, index);
    const prior = previous[index];
    // Preserve a loaded or failed record when the same attachment is re-reported.
    if (prior && prior.path === record.path && prior.mimeType === record.mimeType) return prior;
    return record;
  });
}

function renderToolImages(item) {
  const section = item.imagesSection;
  if (!section) return;
  section.replaceChildren();
  const images = item.images || [];
  if (!images.length) { section.classList.add('hidden'); return; }
  section.classList.remove('hidden');
  const caption = document.createElement('span');
  caption.className = 'tool-caption';
  caption.textContent = images.length === 1 ? 'Image' : `${images.length} images`;
  const grid = document.createElement('div');
  grid.className = 'tool-image-grid';
  for (const record of images) {
    const figure = document.createElement('figure');
    figure.className = 'tool-image';
    if (record.state === 'done' && record.dataUri) {
      const img = document.createElement('img');
      img.className = 'tool-image-img';
      img.src = record.dataUri;
      img.alt = record.caption;
      img.loading = 'lazy';
      figure.appendChild(img);
    } else {
      const placeholder = document.createElement('span');
      placeholder.className = record.state === 'error' ? 'tool-image-error' : 'tool-image-placeholder';
      placeholder.textContent = record.state === 'error'
        ? (record.error || 'Image could not be loaded')
        : 'Loading image…';
      if (record.state === 'error') figure.classList.add('tool-image-failed');
      figure.appendChild(placeholder);
    }
    const path = document.createElement('figcaption');
    path.className = 'tool-image-caption';
    path.textContent = record.caption;
    figure.appendChild(path);
    grid.appendChild(figure);
  }
  section.append(caption, grid);
}

// Fetch lazily, only while the card is expanded. Each record's `state` flips to
// `loading` synchronously, so quick repeated expansions cannot double-load.
async function loadToolImages(item) {
  if (!item || item.collapsed || !item.images?.length) return;
  const epoch = chatEpoch;
  const owner = item;
  let changed = false;
  for (const record of item.images) {
    if (record.state === 'loading' || record.state === 'done') continue;
    changed = true;
    if (!record.path) {
      record.state = 'error';
      record.error = 'Image path is missing';
      continue;
    }
    record.state = 'loading';
    record.error = '';
    renderToolImages(item);
    try {
      const dataUri = await invoke('read_tool_image', { path: record.path });
      if (epoch !== chatEpoch || !chatItems.includes(owner)) return;
      if (typeof dataUri !== 'string' || !dataUri.startsWith('data:')) throw new Error('Image data was not available');
      record.dataUri = dataUri;
      record.state = 'done';
    } catch (err) {
      if (epoch !== chatEpoch || !chatItems.includes(owner)) return;
      record.state = 'error';
      record.error = errorText(err);
    }
  }
  if (changed) renderToolImages(item);
}

function chatSetToolBody(item) {
  const container = item.bodyContainer;
  container.replaceChildren();
  for (const [label, value] of [['Input', item.fullArgs], ['Result', item.result]]) {
    if (value === undefined || value === '') continue;
    const section = document.createElement('section');
    section.className = 'tool-section';
    const caption = document.createElement('span');
    caption.className = 'tool-caption';
    caption.textContent = label;
    const pre = document.createElement('pre');
    pre.className = 'tool-value';
    pre.textContent = label === 'Input' ? formattedToolInput(value) : formattedToolValue(value);
    section.append(caption, pre);
    container.appendChild(section);
  }
  if (item.images?.length) {
    const section = document.createElement('section');
    section.className = 'tool-section tool-images';
    item.imagesSection = section;
    container.appendChild(section);
    renderToolImages(item);
  } else {
    item.imagesSection = null;
  }
  if (item.truncated || item.argsTruncated) {
    const note = document.createElement('span');
    note.className = 'tool-truncated';
    note.textContent = [item.argsTruncated && 'Input', item.truncated && 'result'].filter(Boolean).join(' and ') + ' preview truncated';
    container.appendChild(note);
  }
  const done = item.result !== undefined;
  item.node.classList.toggle('tool-running', !done);
  item.node.classList.toggle('tool-error', done && !!item.isError && !item.cancelled);
  item.node.classList.toggle('tool-done', done && !item.isError && !item.cancelled);
  item.node.classList.toggle('tool-cancelled', !!item.cancelled);
  if (item.statusEl) item.statusEl.textContent = item.cancelled ? 'Stopped' : !done ? 'Running' : item.isError ? 'Error' : 'Done';
  if (!item.collapsed) loadToolImages(item);
}

function onChatEvent(payload) {
  if (!payload) return;
  const text = typeof payload.text === 'string' ? payload.text : '';
  const data = payload.data || {};
  switch (payload.kind) {
    case 'assistant_delta': {
      chatCloseThinking();
      if (!chatStreamingMessage()) chatAppendMessage({ role: 'agent', text: '', streaming: true });
      streamBuffer += text;
      if (!streamTimer) streamTimer = setTimeout(flushStream, 20);
      break;
    }
    case 'thinking_delta': {
      const last = chatItems[chatItems.length - 1];
      const item = last?.kind === 'thinking' && last.streaming
        ? last
        : chatAppendCollapsible({ kind: 'thinking', title: 'Thinking…', collapsed: true, streaming: true });
      chatSetThinking(item, item.text + text);
      break;
    }
    case 'assistant_end': {
      chatCloseThinking();
      if (!chatFinishStreaming(text) && text.trim()) chatAppendMessage({ role: 'agent', text });
      break;
    }
    case 'tool_call': {
      chatCloseThinking();
      chatFinishStreaming();
      const item = chatAppendCollapsible({ kind: 'tool', title: toolLabel(text), args: toolSummary(data.arguments), collapsed: true });
      item.toolName = text;
      item.callId = data.id;
      item.fullArgs = data.arguments ?? '';
      item.argsTruncated = !!data.arguments_truncated;
      setToolImages(item, data.images);
      if (item.argsEl) item.argsEl.title = item.argsEl.textContent;
      chatSetToolBody(item);
      break;
    }
    case 'tool_result': {
      const pending = chatPendingTool(data.id, text);
      const item = pending || chatAppendCollapsible({ kind: 'tool', title: toolLabel(text), collapsed: true });
      item.result = data.result ?? '';
      item.truncated = !!data.truncated;
      setToolImages(item, data.images);
      item.isError = typeof data.is_error === 'boolean'
        ? data.is_error
        : /"isError":true|"error"\s*:/.test(item.result);
      chatSetToolBody(item);
      if (item.isError) chatSetCollapsed(item, false);
      chatFollowOutput();
      break;
    }
    default:
      break;
  }
}

function onAgentMessage(payload) {
  if (!payload?.message) return;
  chatCloseThinking();
  chatFinishStreaming();
  if (payload.kind === 'question') {
    chatAppendMessage({ role: 'agent', kind: 'question', text: payload.message });
    $('chatInput').focus();
  } else if (payload.kind === 'error') {
    chatAppendMessage({ role: 'agent', kind: 'error', text: payload.message });
  } else {
    chatAppendMessage({ role: 'agent', text: payload.message });
  }
}

function resetChat() {
  clearTimeout(streamTimer);
  streamTimer = null;
  streamBuffer = '';
  chatEpoch += 1;
  chatItems = [];
  chatLog().replaceChildren();
  chatFollow = true;
  $('jumpBottom').classList.add('hidden');
}

// Split a replayed tool line (`name args ↳ result`) back into its header and
// argument preview, mirroring how the live tool_call/tool_result pair renders.
function splitToolText(text) {
  const arrow = text.indexOf(' ↳ ');
  const head = arrow === -1 ? text : text.slice(0, arrow);
  const result = arrow === -1 ? '' : text.slice(arrow + 3);
  const space = head.indexOf(' ');
  const name = space === -1 ? head : head.slice(0, space);
  const args = space === -1 ? '' : head.slice(space + 1);
  return { name, args, result };
}

async function loadChatHistory() {
  const version = ++historyVersion;
  let history;
  try {
    history = await invoke('get_chat_history');
  } catch (err) {
    console.warn(err);
    return;
  }
  if (version !== historyVersion) return;
  resetChat();
  for (const entry of history || []) {
    if (entry.kind === 'thinking') {
      const line = chatAppendCollapsible({ kind: 'thinking', title: 'Thinking', collapsed: true });
      chatSetThinking(line, entry.text || '');
    } else if ((entry.role === 'assistant' || entry.role === 'agent') && entry.kind === 'tool') {
      const legacy = splitToolText(entry.text || '');
      const line = chatAppendCollapsible({ kind: 'tool', title: toolLabel(entry.data ? entry.text : legacy.name), args: toolSummary(entry.data?.arguments ?? legacy.args), collapsed: true });
      line.fullArgs = entry.data?.arguments ?? legacy.args;
      line.argsTruncated = !!entry.data?.arguments_truncated;
      line.result = entry.data?.result ?? legacy.result;
      line.isError = !!entry.data?.is_error;
      line.truncated = !!entry.data?.truncated;
      setToolImages(line, entry.data?.images);
      chatSetToolBody(line);
    } else {
      chatAppendMessage({ role: entry.role === 'assistant' ? 'agent' : entry.role, kind: entry.kind === 'agent_message' ? 'message' : entry.kind, text: entry.text });
    }
  }
  chatScrollToBottom();
}

// --- snapshot rendering -----------------------------------------------------
function renderState() {
  if (!snapshot) return;
  applyPlatform(snapshot.platform);

  const folder = snapshot.folder || 'No folder selected';
  $('folderName').textContent = folder;
  $('folderName').title = folder;
  $('chatLog').classList.toggle('empty-unconfigured', snapshot.configured === false);

  const busy = !!snapshot.busy;
  if (!busy) {
    chatCloseThinking();
    chatFinishStreaming();
    for (const item of chatItems) {
      if (item.kind === 'tool' && item.result === undefined) {
        item.cancelled = true;
        item.result = 'Stopped before this tool completed.';
        chatSetToolBody(item);
      }
    }
  }
  const send = $('sendBtn');
  send.classList.toggle('stop', busy);
  send.disabled = !busy && !$('chatInput').value.trim();
  send.setAttribute('aria-label', busy ? 'Stop response' : 'Send message');
  send.title = busy ? 'Stop response' : 'Send message';
}

async function refreshSnapshot() {
  try {
    snapshot = await invoke('get_snapshot');
    renderState();
  } catch (err) {
    console.warn(err);
  }
}

// --- actions ----------------------------------------------------------------
async function stopResponse() {
  try {
    snapshot = await invoke('stop_response');
    renderState();
  } catch (err) {
    showToast(errorText(err));
  }
}

async function chooseFolder() {
  try {
    snapshot = await invoke('choose_folder');
    renderState();
    await loadChatHistory();
  } catch (err) {
    showToast(errorText(err));
  }
}

async function submitMessage() {
  if (snapshot?.busy) return;
  const input = $('chatInput');
  const message = input.value.trim();
  if (!message) return;
  chatFollow = true;
  const optimistic = chatAppendMessage({ role: 'user', text: message });
  const pending = chatAppendCollapsible({ kind: 'thinking', title: 'Thinking…', collapsed: true, streaming: true });
  input.value = '';
  autoGrowInput();
  $('sendBtn').disabled = true;
  try {
    await invoke('submit_user_message', { message });
  } catch (err) {
    pending.node.remove();
    chatItems = chatItems.filter(item => item !== pending);
    optimistic.node.remove();
    chatItems = chatItems.filter(item => item !== optimistic);
    if (!input.value.trim()) { input.value = message; autoGrowInput(); }
    chatAppendMessage({ role: 'agent', kind: 'error', text: errorText(err) });
    input.focus();
  } finally {
    await refreshSnapshot();
    autoGrowInput();
  }
}

function resetClearConfirmation() {
  clearTimeout(clearTimer);
  clearTimer = null;
  $('clearBtn').classList.remove('confirming');
  $('clearBtn').setAttribute('aria-label', 'Clear conversation');
  $('clearBtn').title = 'Clear conversation';
}

async function clearChat() {
  if (!$('clearBtn').classList.contains('confirming')) {
    $('clearBtn').classList.add('confirming');
    $('clearBtn').setAttribute('aria-label', 'Confirm clear conversation');
    $('clearBtn').title = 'Click again to clear conversation';
    clearTimer = setTimeout(resetClearConfirmation, 4000);
    return;
  }
  resetClearConfirmation();
  try {
    await invoke('clear_chat');
    resetChat();
  } catch (err) {
    showToast(errorText(err));
  }
}

// --- settings ---------------------------------------------------------------
// The dialog has two tabs whose settings save independently: model settings go
// to `save_settings`, MCP servers go to `save_mcp_servers`. One footer Save
// button stores the open tab and the header ✕, Escape or a click outside never
// saves.
const SETTINGS_TABS = ['tabLlm', 'tabMcp'];

function panelError(id, message = '') {
  const error = $(id);
  error.textContent = message;
  error.classList.toggle('hidden', !message);
}

function llmError(message = '') { panelError('llmError', message); }
function mcpError(message = '') { panelError('mcpError', message); }

function activeSettingsTab() {
  return $('tabMcp').getAttribute('aria-selected') === 'true' ? 'mcp' : 'llm';
}

function saveShortcut() {
  return snapshot?.platform === 'macos' ? '⌘S' : 'Ctrl+S';
}

// The footer action is associated with the open tab's form, so Enter in a
// single-line field submits the same way. It is disabled while a save is in
// flight and while the MCP tab is still loading its configuration.
function updateSaveButton() {
  const save = $('settingsSave');
  const mcp = activeSettingsTab() === 'mcp';
  save.setAttribute('form', mcp ? 'mcpForm' : 'llmForm');
  save.disabled = settingsSaving || (mcp && mcpLoading);
  const label = mcp ? 'Save MCP servers' : 'Save LLM settings';
  save.title = `${label} (${saveShortcut()})`;
  save.setAttribute('aria-label', label);
}

function setSaving(on) {
  settingsSaving = on;
  updateSaveButton();
}

// Roving-tabindex tabs: only the selected tab is in the Tab order, and the
// Arrow/Home/End keys move between tabs (WAI-ARIA tabs pattern).
function activateTab(id, { focus = true } = {}) {
  for (const tabId of SETTINGS_TABS) {
    const tab = $(tabId);
    const active = tabId === id;
    tab.setAttribute('aria-selected', String(active));
    tab.tabIndex = active ? 0 : -1;
    $(tab.getAttribute('aria-controls')).classList.toggle('hidden', !active);
  }
  updateSaveButton();
  if (focus) $(id).focus();
}

function onTabKeydown(event) {
  const current = SETTINGS_TABS.indexOf(document.activeElement?.id);
  if (current === -1) return;
  let next = null;
  if (event.key === 'ArrowRight') next = (current + 1) % SETTINGS_TABS.length;
  else if (event.key === 'ArrowLeft') next = (current - 1 + SETTINGS_TABS.length) % SETTINGS_TABS.length;
  else if (event.key === 'Home') next = 0;
  else if (event.key === 'End') next = SETTINGS_TABS.length - 1;
  if (next === null) return;
  event.preventDefault();
  activateTab(SETTINGS_TABS[next]);
}

function updateMcpEmptyState() {
  const count = $('mcpServers').children.length;
  $('mcpEmpty').classList.toggle('hidden', count > 0);
  $('addMcpBtn').disabled = !mcpConfigLoaded || count >= MAX_MCP_SERVERS;
  $('inspectMcpBtn').disabled = !mcpConfigLoaded || mcpDirty;
}

function markMcpEdited() {
  mcpDirty = true;
  toolCatalogEpoch += 1;
  $('mcpCatalog').replaceChildren();
  $('mcpCatalogStatus').textContent = 'Save changes before inspecting available tools.';
  updateMcpEmptyState();
}

function renderToolCatalog(reports) {
  const container = $('mcpCatalog');
  container.replaceChildren();
  for (const report of reports) {
    const card = document.createElement('section');
    card.className = 'mcp-catalog-server';
    const heading = document.createElement('div');
    heading.className = 'mcp-catalog-heading';
    const name = document.createElement('strong');
    name.textContent = report.name;
    const status = document.createElement('span');
    status.className = `mcp-catalog-status status-${report.status}`;
    status.textContent = report.status === 'ok' ? `${report.tools.length} tools` : report.status === 'disabled' ? 'Disabled' : 'Connection failed';
    heading.append(name, status);
    card.appendChild(heading);
    if (report.error) {
      const error = document.createElement('p');
      error.className = 'mcp-catalog-error';
      error.textContent = report.error;
      card.appendChild(error);
    } else if (report.status === 'ok' && !report.tools.length) {
      const empty = document.createElement('p');
      empty.className = 'mcp-hint';
      empty.textContent = 'This server did not advertise any tools.';
      card.appendChild(empty);
    }
    for (const tool of report.tools || []) {
      const details = document.createElement('details');
      details.className = 'mcp-catalog-tool';
      const summary = document.createElement('summary');
      summary.textContent = tool.name;
      details.appendChild(summary);
      if (tool.description) {
        const description = document.createElement('p');
        description.textContent = tool.description;
        details.appendChild(description);
      }
      const schema = document.createElement('pre');
      schema.textContent = JSON.stringify(tool.input_schema || {}, null, 2);
      details.appendChild(schema);
      card.appendChild(details);
    }
    container.appendChild(card);
  }
}

async function inspectMcpTools() {
  const button = $('inspectMcpBtn');
  const epoch = ++toolCatalogEpoch;
  const settingsVersion = settingsEpoch;
  button.disabled = true;
  $('mcpCatalogStatus').textContent = 'Connecting to enabled servers and reading their tool lists…';
  try {
    const reports = await invoke('list_mcp_tools');
    if (epoch !== toolCatalogEpoch || settingsVersion !== settingsEpoch) return;
    renderToolCatalog(reports);
    $('mcpCatalogStatus').textContent = reports.length ? 'Tool catalog from saved configuration. Expand a tool to view its input schema.' : 'No MCP servers configured.';
  } catch (error) {
    if (epoch === toolCatalogEpoch && settingsVersion === settingsEpoch) $('mcpCatalogStatus').textContent = `Could not inspect tools: ${errorText(error)}`;
  } finally {
    if (epoch === toolCatalogEpoch && settingsVersion === settingsEpoch) button.disabled = !mcpConfigLoaded || mcpDirty;
  }
}

function mcpField(caption, control, className) {
  const field = document.createElement('label');
  field.className = `field ${className}`;
  const heading = document.createElement('span');
  heading.className = 'field-label';
  heading.textContent = caption;
  field.append(heading, control);
  return field;
}

function mcpInput(className, label) {
  const input = document.createElement('input');
  input.type = 'text';
  input.className = `mcp-input ${className}`;
  input.spellcheck = false;
  input.autocorrect = 'off';
  input.autocapitalize = 'off';
  input.setAttribute('aria-label', label);
  return input;
}

function mcpTextarea(className, label, placeholder) {
  const area = document.createElement('textarea');
  area.className = `mcp-input mcp-textarea ${className}`;
  area.rows = 3;
  area.spellcheck = false;
  area.autocorrect = 'off';
  area.autocapitalize = 'off';
  area.setAttribute('aria-label', label);
  if (placeholder) area.placeholder = placeholder;
  return area;
}

// The connection field determines which settings are relevant as it is typed.
function addMcpRow(seed = {}) {
  const row = normalizeServer(seed);
  const node = document.createElement('div');
  node.className = 'mcp-row';

  const header = document.createElement('div');
  header.className = 'mcp-row-header';
  const toggle = document.createElement('input');
  toggle.type = 'checkbox';
  toggle.className = 'mcp-enabled';
  toggle.checked = row.enabled;
  const switchTrack = document.createElement('span');
  switchTrack.className = 'switch-track';
  const switchLabel = document.createElement('label');
  switchLabel.className = 'switch-label';
  const title = document.createElement('span');
  title.className = 'mcp-row-title';
  title.textContent = row.name || 'New server';
  switchLabel.append(toggle, switchTrack, title);
  toggle.setAttribute('aria-label', `Enable ${row.name || 'MCP server'}`);

  const remove = document.createElement('button');
  remove.type = 'button';
  remove.className = 'remove-mcp icon-btn';
  remove.innerHTML = '<svg class="nav-icon" viewBox="0 0 20 20" aria-hidden="true"><path d="M4 6h12M8.5 6V4.5h3V6M6.5 6l.7 9.5h5.6l.7-9.5M9 8.75v4.5M11 8.75v4.5"/></svg>';
  remove.title = `Remove ${row.name || 'server'}`;
  remove.setAttribute('aria-label', remove.title);
  header.append(switchLabel, remove);

  const name = mcpInput('mcp-name', 'MCP server name');
  name.placeholder = 'server_name';
  name.value = row.name;

  const connection = mcpInput('mcp-connection', 'Server command or URL');
  connection.placeholder = 'Executable or https://…';
  connection.value = row.connection;
  const detected = document.createElement('span');
  detected.className = 'mcp-detected';
  detected.setAttribute('aria-live', 'polite');

  const args = mcpTextarea('mcp-args', 'Arguments, one per line', `One argument per line\n${WORKSPACE_TOKEN}`);
  args.value = argumentsToText(row.args);

  const env = mcpTextarea('mcp-env', 'Environment variables, KEY=value per line', 'KEY=value');
  env.value = envToText(row.env);

  const connectionField = mcpField('Command or URL', connection, 'mcp-connection-field');
  const argsField = mcpField('Arguments · one per line', args, 'mcp-args-field');
  const insertFolder = document.createElement('button');
  insertFolder.type = 'button';
  insertFolder.className = 'mcp-insert-folder';
  insertFolder.textContent = '＋ Working folder';
  insertFolder.title = 'Add the selected folder as one argument';
  insertFolder.addEventListener('click', () => {
    args.value = args.value.trimEnd() ? `${args.value.trimEnd()}\n${WORKSPACE_TOKEN}` : WORKSPACE_TOKEN;
    args.focus();
  });
  const folderAction = document.createElement('div');
  folderAction.className = 'mcp-folder-action';
  folderAction.appendChild(insertFolder);
  const envField = mcpField('Environment · KEY=value per line', env, 'mcp-env-field');

  const fields = document.createElement('div');
  fields.className = 'mcp-row-fields';
  fields.append(
    mcpField('Name', name, 'mcp-name-field'),
    connectionField, detected, argsField, folderAction, envField,
  );

  const syncConnection = () => {
    const remote = isRemoteConnection(connection.value);
    node.dataset.type = remote ? 'remote' : 'local';
    detected.textContent = remote ? 'Remote · Streamable HTTP' : 'Local · stdio';
    argsField.classList.toggle('hidden', remote);
    folderAction.classList.toggle('hidden', remote);
    envField.classList.toggle('hidden', remote);
  };
  connection.addEventListener('input', syncConnection);

  const rename = () => {
    const value = name.value.trim();
    title.textContent = value || 'New server';
    toggle.setAttribute('aria-label', `Enable ${value || 'MCP server'}`);
    remove.title = `Remove ${value || 'server'}`;
    remove.setAttribute('aria-label', remove.title);
  };
  name.addEventListener('input', rename);
  remove.addEventListener('click', () => {
    node.remove();
    updateMcpEmptyState();
  });

  const editor = document.createElement('details');
  editor.className = 'mcp-editor';
  editor.open = !row.name;
  const editorToggle = document.createElement('summary');
  editorToggle.textContent = row.connection || 'Configure connection';
  const syncSummary = () => { editorToggle.textContent = connection.value.trim() || 'Configure connection'; };
  connection.addEventListener('input', syncSummary);
  editor.append(editorToggle, fields);
  node.append(header, editor);
  node.addEventListener('input', markMcpEdited);
  node.addEventListener('change', markMcpEdited);
  remove.addEventListener('click', markMcpEdited);
  $('mcpServers').appendChild(node);
  syncConnection();
  updateMcpEmptyState();
  return node;
}

// Read every row back into the editable model, keeping a few DOM/error details
// alongside so save can report the first problem precisely.
function collectMcpEntries() {
  return Array.from($('mcpServers').querySelectorAll('.mcp-row'), node => {
    const { env, invalid } = parseEnvLines(node.querySelector('.mcp-env')?.value ?? '');
    return {
      node,
      envInvalid: invalid,
      row: {
        name: node.querySelector('.mcp-name').value,
        enabled: node.querySelector('.mcp-enabled').checked,
        connection: node.querySelector('.mcp-connection').value,
        args: parseArgumentLines(node.querySelector('.mcp-args')?.value ?? ''),
        env,
      },
    };
  });
}

function focusRowField(entry, field) {
  if (entry) entry.node.querySelector('.mcp-editor').open = true;
  const target = entry?.node.querySelector(`.mcp-${field}`) || entry?.node.querySelector('input, textarea');
  target?.focus();
}

function renderApiKeyState(settings = {}) {
  $('setApiKey').value = '';
  $('clearApiKey').checked = false;
  $('setApiKey').disabled = false;
  $('clearKeyOption').classList.toggle('hidden', !settings.saved_api_key_set);
  $('setApiKey').placeholder = settings.openai_api_key_set ? 'Leave blank to keep the current key' : 'Optional for local endpoints';
}

function renderLlmForm() {
  const settings = snapshot?.settings || {};
  $('setBaseUrl').value = settings.openai_base_url || '';
  $('setModel').value = settings.openai_model || '';
  renderApiKeyState(settings);
  const envFields = settings.env_fields || [];
  $('envNotice').textContent = envFields.length ? `Using ${envFields.join(', ')} from this folder’s .env. A saved key is never sent to a URL from this folder.` : 'Tip: Add OPENAI_BASE_URL and OPENAI_MODEL to this folder’s .env; OPENAI_API_KEY is optional for local endpoints.';
  $('envNotice').classList.remove('hidden');
  $('setSystemPrompt').value = snapshot?.system_prompt || '';
  llmError();
}

async function openSettings() {
  if (!snapshot) return;
  const epoch = ++settingsEpoch;
  renderLlmForm();
  mcpError();
  setSaving(false);
  activateTab('tabLlm', { focus: false });
  $('appShell').inert = true;
  $('settingsOverlay').classList.remove('hidden');
  mcpConfigLoaded = false;
  mcpDirty = false;
  mcpLoading = true;
  updateSaveButton();
  $('mcpServers').replaceChildren();
  $('mcpCatalog').replaceChildren();
  $('mcpCatalogStatus').textContent = 'Save changes before inspecting. Inspection connects to enabled servers and may open a browser for authorization.';
  updateMcpEmptyState();
  $('setBaseUrl').focus();
  try {
    const config = await invoke('get_mcp_servers');
    if (epoch !== settingsEpoch || $('settingsOverlay').classList.contains('hidden')) return;
    for (const server of Array.isArray(config?.servers) ? config.servers : []) addMcpRow(server);
    mcpConfigLoaded = true;
  } catch (error) {
    if (epoch === settingsEpoch) mcpError(errorText(error));
  } finally {
    if (epoch === settingsEpoch) {
      mcpLoading = false;
      updateSaveButton();
      updateMcpEmptyState();
    }
  }
}

// Bumping the epoch retires any in-flight configuration load, so reopening the
// dialog never mixes the previous load into the new one.
function closeSettings() {
  settingsEpoch += 1;
  toolCatalogEpoch += 1;
  $('settingsOverlay').classList.add('hidden');
  $('appShell').inert = false;
  $('settingsBtn').focus();
}

async function saveLlm(event) {
  event.preventDefault();
  if (settingsSaving) return;
  llmError();
  const settings = {
    openai_base_url: $('setBaseUrl').value.trim(),
    openai_model: $('setModel').value.trim(),
    // An empty key is sent as-is: the backend keeps the stored value.
    openai_api_key: $('setApiKey').value,
    clear_api_key: $('clearApiKey').checked,
    system_prompt: $('setSystemPrompt').value,
  };
  setSaving(true);
  try {
    snapshot = await invoke('save_settings', { settings });
    renderState();
    renderApiKeyState(snapshot.settings || {});
    showToast('LLM settings saved');
  } catch (err) {
    llmError(errorText(err));
  } finally {
    setSaving(false);
  }
}

async function saveMcp(event) {
  event.preventDefault();
  if (settingsSaving) return;
  if (!mcpConfigLoaded) { mcpError('MCP servers could not be loaded; fix mcp.json before saving'); return; }
  mcpError();
  const entries = collectMcpEntries();
  const badEnv = entries.find(entry => entry.envInvalid.length);
  if (badEnv) {
    mcpError(`Invalid environment variable name: ${badEnv.envInvalid[0]}`);
    badEnv.node.querySelector('.mcp-editor').open = true;
    badEnv.node.querySelector('.mcp-env').focus();
    return;
  }
  const { index, field, error } = validateRows(entries.map(entry => entry.row));
  if (error) {
    mcpError(error);
    focusRowField(entries[index], field);
    return;
  }
  const config = { servers: entries.map(entry => rowToServer(entry.row)) };
  setSaving(true);
  try {
    const saved = await invoke('save_mcp_servers', { config });
    $('mcpServers').replaceChildren();
    for (const server of saved.servers || []) addMcpRow(server);
    mcpDirty = false;
    updateMcpEmptyState();
    toolCatalogEpoch += 1;
    $('mcpCatalog').replaceChildren();
    $('mcpCatalogStatus').textContent = 'Saved. Inspect available tools to check connections and browse the catalog.';
    showToast('MCP servers saved');
  } catch (err) {
    mcpError(errorText(err));
  } finally {
    setSaving(false);
  }
}

// --- composer ---------------------------------------------------------------
function autoGrowInput() {
  const input = $('chatInput');
  input.style.height = 'auto';
  input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
  $('sendBtn').disabled = !snapshot?.busy && input.value.trim().length === 0;
}

// --- events -----------------------------------------------------------------
async function setupEvents() {
  if (!listen) return;
  await listen('state-changed', () => { refreshSnapshot(); });
  await listen('folder-changed', async () => {
    await refreshSnapshot();
    await loadChatHistory();
  });
  await listen('chat-cleared', () => { resetChat(); });
  await listen('agent-message', event => onAgentMessage(event.payload));
  await listen('agent-chat', event => onChatEvent(event.payload));
}

// --- wiring -----------------------------------------------------------------
function wireWindowControls() {
  const controls = [
    ['macClose', () => appWindow?.close()],
    ['winClose', () => appWindow?.close()],
    ['macMin', () => appWindow?.minimize()],
    ['winMin', () => appWindow?.minimize()],
    ['macMax', async () => {
      if (!appWindow) return;
      const fullscreen = !(await appWindow.isFullscreen());
      await appWindow.setFullscreen(fullscreen);
      document.documentElement.classList.toggle('fullscreen', fullscreen);
    }],
    ['winMax', () => appWindow?.toggleMaximize()],
  ];
  for (const [id, action] of controls) {
    $(id)?.addEventListener('click', action);
  }
  // Double-clicking the drag region zooms, like a native title bar.
  $('titlebar')?.addEventListener('dblclick', event => {
    if (event.target.closest('button, input, textarea, .window-no-drag')) return;
    appWindow?.toggleMaximize();
  });
}

function wireUi() {
  wireWindowControls();
  $('folderInfo').addEventListener('click', chooseFolder);
  $('settingsBtn').addEventListener('click', openSettings);
  $('tabLlm').addEventListener('click', () => activateTab('tabLlm'));
  $('tabMcp').addEventListener('click', () => activateTab('tabMcp'));
  $('tabLlm').closest('.settings-tabs').addEventListener('keydown', onTabKeydown);
  $('addMcpBtn').addEventListener('click', () => {
    if ($('mcpServers').children.length >= MAX_MCP_SERVERS) { mcpError(`At most ${MAX_MCP_SERVERS} MCP servers are supported.`); return; }
    mcpError();
    addMcpRow().querySelector('.mcp-name').focus();
    markMcpEdited();
  });
  $('inspectMcpBtn').addEventListener('click', inspectMcpTools);
  $('exportChatBtn').addEventListener('click', async () => {
    try {
      const exported = await invoke('export_chat');
      if (exported) showToast('Conversation exported');
    } catch (err) { showToast(errorText(err)); }
  });
  $('settingsClose').addEventListener('click', closeSettings);
  $('clearBtn').addEventListener('click', clearChat);
  $('clearBtn').addEventListener('blur', resetClearConfirmation);
  $('llmForm').addEventListener('submit', saveLlm);
  $('clearApiKey').addEventListener('change', () => {
    $('setApiKey').disabled = $('clearApiKey').checked;
    if ($('clearApiKey').checked) $('setApiKey').value = '';
  });
  $('mcpForm').addEventListener('submit', saveMcp);
  $('settingsOverlay').addEventListener('click', event => {
    if (event.target === $('settingsOverlay')) closeSettings();
  });

  $('composer').addEventListener('submit', event => {
    event.preventDefault();
    if (snapshot?.busy) stopResponse(); else submitMessage();
  });
  const input = $('chatInput');
  input.addEventListener('input', autoGrowInput);
  input.addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      if (!snapshot?.busy) submitMessage();
    }
  });
  chatLog().addEventListener('scroll', () => {
    const log = chatLog();
    chatFollow = log.scrollTop + log.clientHeight >= log.scrollHeight - 24;
    $('jumpBottom').classList.toggle('hidden', chatFollow);
  });
  $('jumpBottom').addEventListener('click', () => { chatFollow = true; chatScrollToBottom(); $('jumpBottom').classList.add('hidden'); });

  document.addEventListener('keydown', event => {
    if ((event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey && event.key.toLowerCase() === 's' && !$('settingsOverlay').classList.contains('hidden')) {
      event.preventDefault();
      $('settingsSave').click();
      return;
    }
    if (event.key === 'Tab' && !$('settingsOverlay').classList.contains('hidden')) {
      // Skip hidden tab panels, disabled controls and inactive tabs (roving
      // tabindex -1) so the focus trap stays on what is actually reachable.
      const focusable = Array.from($('settingsOverlay').querySelectorAll('button, input, textarea, [tabindex]:not([tabindex="-1"])'))
        .filter(el => !el.disabled && el.type !== 'hidden' && el.getAttribute('tabindex') !== '-1' && el.offsetParent !== null);
      const first = focusable[0], last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
    }
    if (event.key === 'Escape') resetClearConfirmation();
    if (event.key === 'Escape' && !$('settingsOverlay').classList.contains('hidden')) {
      closeSettings();
    }
  });
}

async function boot() {
  wireUi();
  await refreshSnapshot();
  await loadChatHistory();
  await setupEvents();
  // WKWebView can repeatedly wake macOS's predictive-text service when a
  // textarea is focused on launch. Let macOS users focus the composer when
  // they actually want to type; other platforms keep the quick-start focus.
  if (snapshot?.platform !== 'macos') $('chatInput').focus();
}

if (isBrowser) {
  boot().catch(err => {
    console.error(err);
    showToast(errorText(err));
  });
}

// Pure helpers are exported so the UI logic can be unit-tested without a DOM.
export {
  MAX_MCP_SERVERS,
  WORKSPACE_TOKEN,
  parseArgumentLines,
  argumentsToText,
  parseEnvLines,
  envToText,
  isRemoteConnection,
  normalizeServer,
  rowToServer,
  remoteUrlError,
  validateRows,
};
