'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const { executable, npmCli, launchEnv } = require('../bin/flodo.cjs');

test('selects the supported native executable, including a signed macOS bundle', () => {
  assert.equal(executable('win32', 'x64', '/pkg'), path.join('/pkg', 'native', 'win32', 'flodo.exe'));
  assert.equal(executable('linux', 'x64', '/pkg'), path.join('/pkg', 'native', 'linux', 'flodo'));
  assert.equal(executable('darwin', 'arm64', '/pkg'), executable('darwin', 'x64', '/pkg'));
  assert.match(executable('darwin', 'arm64'), /Flodo\.app/);
});

test('rejects unsupported architectures instead of launching the wrong binary', () => {
  assert.throws(() => executable('linux', 'arm64'), /Unsupported platform/);
  assert.throws(() => executable('win32', 'ia32'), /Unsupported platform/);
  assert.throws(() => executable('freebsd', 'x64'), /Unsupported platform/);
});

test('finds the npm CLI that ships beside node on each platform', () => {
  const seen = [];
  const exists = (file) => { seen.push(file); return true; };
  assert.equal(
    npmCli('win32', path.join('C:', 'nodejs', 'node.exe'), exists),
    path.resolve(path.join('C:', 'nodejs', 'node_modules', 'npm', 'bin', 'npm-cli.js')));
  assert.equal(
    npmCli('linux', path.join('/usr', 'bin', 'node'), exists),
    path.resolve(path.join('/usr', 'lib', 'node_modules', 'npm', 'bin', 'npm-cli.js')));
  assert.equal(npmCli('darwin', '/opt/homebrew/bin/node', () => false), null);
  assert.equal(seen.length, 2);
});

test('passes node and npm to the app without dropping the environment', () => {
  const env = launchEnv({ FLODO_STATE_DIR: '/tmp/s', FLODO_NPM_CLI: '/stale' }, 'linux', '/usr/bin/node', () => false);
  assert.equal(env.FLODO_STATE_DIR, '/tmp/s');
  assert.equal(env.FLODO_NODE, '/usr/bin/node');
  assert.equal('FLODO_NPM_CLI' in env, false);
  const found = launchEnv({}, 'linux', '/usr/bin/node', () => true);
  assert.match(found.FLODO_NPM_CLI, /npm-cli\.js$/);
});
