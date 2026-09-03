#!/usr/bin/env node
'use strict';

const path = require('node:path');
const { spawn } = require('node:child_process');

const binary = process.env.CFY_BINARY_PATH || path.join(
  __dirname,
  '..',
  'vendor',
  process.platform === 'win32' ? 'cfy.exe' : 'cfy',
);

const child = spawn(binary, process.argv.slice(2), {
  stdio: 'inherit',
  env: process.env,
});

for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(signal, () => {
    if (!child.killed) child.kill(signal);
  });
}

child.on('error', (error) => {
  console.error(`catify-cli could not start ${binary}: ${error.message}`);
  console.error('Reinstall with `npm install --global catify-cli`.');
  process.exit(1);
});

child.on('exit', (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
