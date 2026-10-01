'use strict';

const { readFileSync, statSync } = require('node:fs');
const path = require('node:path');
const { executable } = require('../bin/flodo.cjs');
const { version } = require('../package.json');
const cargo = readFileSync(path.join(__dirname, '..', 'Cargo.toml'), 'utf8');
if (!cargo.includes(`version = "${version}"`)) throw new Error('Cargo and npm versions differ');
for (const platform of ['win32', 'linux', 'darwin']) {
  const file = executable(platform, 'x64');
  if (!statSync(file).isFile() || statSync(file).size < 100000) {
    throw new Error(`Missing or invalid native executable: ${file}`);
  }
}
console.log(`Flodo ${version}: all three native packages are present.`);
