import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { inlineTokens } from '../src/markdown.js';

import {
  MAX_MCP_SERVERS,
  WORKSPACE_TOKEN,
  argumentsToText,
  envToText,
  isRemoteConnection,
  normalizeServer,
  parseArgumentLines,
  parseEnvLines,
  remoteUrlError,
  rowToServer,
  validateRows,
} from '../src/app.js';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const html = readFileSync(join(root, 'src', 'index.html'), 'utf8');
const app = readFileSync(join(root, 'src', 'app.js'), 'utf8');

test('arguments remain separate and are never shell-split', () => {
  const args = ['--flag value', '"literal quotes"', WORKSPACE_TOKEN];
  assert.deepEqual(parseArgumentLines(argumentsToText(args)), args);
  assert.deepEqual(parseArgumentLines(' a\n\n b c '), ['a', 'b c']);
});

test('minimal Markdown keeps raw HTML and URLs inert', () => {
  assert.deepEqual(inlineTokens('**bold** and `code` *em*'), [
    { type: 'strong', text: 'bold' }, { type: 'text', text: ' and ' },
    { type: 'code', text: 'code' }, { type: 'text', text: ' ' }, { type: 'em', text: 'em' },
  ]);
  assert.deepEqual(inlineTokens('<img src=x onerror=alert(1)> [link](javascript:alert(1))'), [
    { type: 'text', text: '<img src=x onerror=alert(1)> [link](javascript:alert(1))' },
  ]);
});

test('environment values can contain equals signs', () => {
  const original = { TOKEN: 'x=y', EMPTY: '' };
  assert.deepEqual(parseEnvLines(envToText(original)), { env: original, invalid: [] });
  assert.deepEqual(parseEnvLines('BAD-KEY=1\nMISSING\n# comment').invalid, ['BAD-KEY=1', 'MISSING']);
});

test('connection detection recognizes URL schemes, not executable names', () => {
  for (const value of ['https://example.com/mcp', 'http://localhost:3000/mcp', 'ftp://example.com', 'https:/typo']) {
    assert.equal(isRemoteConnection(value), true, value);
  }
  for (const value of ['python', 'npx', 'uvx', 'C:\\Program Files\\server.exe', '/opt/my server', '']) {
    assert.equal(isRemoteConnection(value), false, value);
  }
});

test('normalization preserves arbitrary executable, arguments, env and disabled state', () => {
  assert.deepEqual(normalizeServer({ name: 'local', command: '  python  ', args: ['server.py'], env: { KEY: 'value' }, enabled: false }), {
    name: 'local', connection: 'python', args: ['server.py'], env: { KEY: 'value' }, enabled: false,
  });
  assert.equal(normalizeServer({ name: 'remote', url: 'https://example.com/mcp' }).connection, 'https://example.com/mcp');
  assert.equal(normalizeServer({}).connection, '');
});

test('local save keeps the typed command verbatim and never invents a package manager', () => {
  const row = { name: 'local', connection: 'my-runtime', args: ['server', WORKSPACE_TOKEN], env: { ROOT: WORKSPACE_TOKEN }, enabled: true };
  assert.deepEqual(rowToServer(row), {
    name: 'local', url: '', command: 'my-runtime', args: row.args, env: row.env, enabled: true,
  });
  assert.deepEqual(rowToServer(normalizeServer(rowToServer(row))), rowToServer(row));
});

test('remote save removes stale local-only settings', () => {
  assert.deepEqual(rowToServer({ name: 'r', connection: ' https://example.com/mcp ', args: ['stale'], env: { STALE: 'x' }, enabled: false }), {
    name: 'r', url: 'https://example.com/mcp', args: [], env: {}, enabled: false,
  });
});

test('remote URL validation matches backend transport rules', () => {
  for (const url of ['https://example.com/mcp', 'http://localhost:3000/mcp', 'http://127.0.0.1:3000/mcp']) {
    assert.equal(remoteUrlError(url), '');
  }
  for (const url of ['https:/typo', 'ftp://example.com', 'http://example.com/mcp', 'https://user:pass@example.com', 'https://example.com/#frag']) {
    assert.notEqual(remoteUrlError(url), '', url);
  }
});

test('rows validate the detected connection and unique names', () => {
  const rows = [{ name: 'local', connection: 'python' }, { name: 'remote', connection: 'https://example.com/mcp' }];
  assert.deepEqual(validateRows(rows), { index: -1, field: '', error: '' });
  assert.equal(validateRows([{ name: 'local', connection: '' }]).field, 'connection');
  assert.equal(validateRows([{ name: 'remote', connection: 'http://example.com/mcp' }]).field, 'connection');
  assert.equal(validateRows([{ name: 'bad name', connection: 'python' }]).field, 'name');
  assert.equal(validateRows([rows[0], rows[0]]).index, 1);
  const maximum = Array.from({ length: MAX_MCP_SERVERS }, (_, i) => ({ name: `s${i}`, connection: 'server' }));
  assert.equal(validateRows(maximum).error, '');
  assert.match(validateRows([...maximum, { name: 'extra', connection: 'server' }]).error, /at most/i);
});

test('settings has two tabs, no runtime select or bundled server preset', () => {
  assert.match(html, /role="tablist"/);
  assert.match(html, /id="panelLlm"[^>]*role="tabpanel"/);
  assert.match(html, /id="panelMcp"[^>]*role="tabpanel"/);
  assert.match(html, /No MCP servers yet/i);
  assert.ok(html.includes(WORKSPACE_TOKEN));
  assert.match(app, /connection\.addEventListener\('input', syncConnection\)/);
  assert.match(app, /invoke\('save_mcp_servers', \{ config \}\)/);
  assert.match(app, /invoke\('save_settings', \{ settings \}\)/);
  for (const source of [app, html]) {
    assert.doesNotMatch(source, /<select|createElement\('select'\)|server-filesystem|openrouter|@modelcontextprotocol/i);
  }
});

test('one footer Save action saves the open settings tab', () => {
  assert.match(html, /id="exportChatBtn"[^>]*>[\s\S]*?Export chat<\/button>/);
  assert.match(html, /id="settingsSave"[^>]*type="submit"[^>]*form="llmForm"[^>]*>Save<\/button>/);
  assert.doesNotMatch(html, /id="llmSave"|id="mcpSave"|id="settingsCancel"|settings-tab-actions/);
  assert.match(app, /save\.setAttribute\('form', mcp \? 'mcpForm' : 'llmForm'\)/);
  assert.match(app, /event\.key\.toLowerCase\(\) === 's'/);
  assert.doesNotMatch(app, /llmSave|mcpSave|settingsCancel/);
});
