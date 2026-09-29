import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

// Release workflow tags must match every manifest. Keeping the three files in
// sync here avoids shipping a bundle whose version disagrees with its tag.
const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const read = relative => readFileSync(join(root, relative), 'utf8');

const packageJson = JSON.parse(read('package.json'));
const tauriConfig = JSON.parse(read('src-tauri/tauri.conf.json'));
const cargoToml = read('src-tauri/Cargo.toml');
const cargoLock = read('src-tauri/Cargo.lock');

test('package.json, tauri.conf.json and Cargo.toml share one version', () => {
  const crate = cargoToml.match(/^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m)?.[1];
  assert.ok(crate, 'Cargo.toml package.version is missing');
  assert.equal(tauriConfig.version, packageJson.version);
  assert.equal(crate, packageJson.version);
  assert.equal(tauriConfig.productName, packageJson.name);
});

test('Cargo.lock pins the same datasetop version', () => {
  const locked = cargoLock.match(/^\[\[package\]\]\nname = "datasetop"\nversion = "([^"]+)"/m)?.[1];
  assert.equal(locked, packageJson.version);
});
