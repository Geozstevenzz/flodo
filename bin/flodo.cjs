#!/usr/bin/env node
'use strict';

const { spawn } = require('node:child_process');
const { existsSync } = require('node:fs');
const path = require('node:path');

function executable(platform, arch, root = path.join(__dirname, '..')) {
  if (platform === 'darwin' && ['x64', 'arm64'].includes(arch)) {
    return path.join(root, 'native', 'darwin', 'Flodo.app', 'Contents', 'MacOS', 'flodo');
  }
  if (arch === 'x64' && ['win32', 'linux'].includes(platform)) {
    return path.join(root, 'native', platform, platform === 'win32' ? 'flodo.exe' : 'flodo');
  }
  throw new Error(`Unsupported platform ${platform}/${arch}. Flodo supports Windows x64, Linux x64 (glibc), and macOS Intel/Apple Silicon.`);
}

// The npm CLI that ships with this node, so the app's update button can run
// npm without relying on PATH. A GUI app started from a shortcut or a desktop
// menu often has a PATH without npm on it.
function npmCli(platform, execPath, exists = existsSync) {
  const dir = path.dirname(execPath);
  const cli = platform === 'win32'
    ? path.join(dir, 'node_modules', 'npm', 'bin', 'npm-cli.js')
    : path.join(dir, '..', 'lib', 'node_modules', 'npm', 'bin', 'npm-cli.js');
  return exists(cli) ? path.resolve(cli) : null;
}

function launchEnv(env, platform = process.platform, execPath = process.execPath, exists = existsSync) {
  const cli = npmCli(platform, execPath, exists);
  const out = { ...env, FLODO_NODE: execPath };
  if (cli) out.FLODO_NPM_CLI = cli;
  else delete out.FLODO_NPM_CLI;
  return out;
}

function launch(args) {
  const binary = executable(process.platform, process.arch);
  if (!existsSync(binary)) {
    throw new Error('The native executable is missing. Reinstall the published flodo npm package.');
  }
  const gui = args.length === 0;
  const macGui = gui && process.platform === 'darwin';
  const child = spawn(macGui ? '/usr/bin/open' : binary,
    macGui ? ['-a', path.resolve(binary, '..', '..', '..')] : args, {
      // Keep the inherited environment, including FLODO_STATE_DIR and APPDATA,
      // so this fork opens the legacy application's existing files.
      env: launchEnv(process.env),
      windowsHide: true,
      detached: gui,
      // A Windows GUI-subsystem executable needs pipes for its CLI output.
      stdio: gui ? 'ignore' : ['inherit', 'pipe', 'pipe'],
    });
  child.on('error', (error) => {
    console.error(`flodo: ${error.message}`);
    process.exitCode = 1;
  });
  if (gui) {
    child.unref();
  } else {
    child.stdout.pipe(process.stdout);
    child.stderr.pipe(process.stderr);
    child.on('exit', (code) => { process.exitCode = code ?? 1; });
  }
}

if (require.main === module) {
  try { launch(process.argv.slice(2)); }
  catch (error) {
    console.error(`flodo: ${error.message}`);
    process.exitCode = 1;
  }
}

module.exports = { executable, npmCli, launchEnv };
