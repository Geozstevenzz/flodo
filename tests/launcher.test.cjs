'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const { executable } = require('../bin/flodo.cjs');

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
