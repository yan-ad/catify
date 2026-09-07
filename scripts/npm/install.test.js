'use strict';

const assert = require('node:assert/strict');
const test = require('node:test');
const { checksumFromManifest, targetFor } = require('./install');
const { requiredAssets, validateRelease } = require('../check-release-assets');

test('maps supported npm platforms to Rust targets', () => {
  assert.equal(targetFor('darwin', 'arm64'), 'aarch64-apple-darwin');
  assert.equal(targetFor('darwin', 'x64'), 'x86_64-apple-darwin');
  assert.equal(targetFor('linux', 'arm64'), 'aarch64-unknown-linux-gnu');
  assert.equal(targetFor('linux', 'x64'), 'x86_64-unknown-linux-gnu');
  assert.equal(targetFor('win32', 'x64'), 'x86_64-pc-windows-msvc');
  assert.throws(() => targetFor('linux', 'arm'), /unsupported platform/);
});

test('npm publish preflight requires every platform asset', () => {
  const version = '0.0.1-pre.0';
  const assets = requiredAssets(version).map((name) => ({ name }));
  assert.doesNotThrow(() => validateRelease({ assets }, version));
  assert.throws(
    () => validateRelease({ assets: assets.slice(1) }, version),
    /missing assets/,
  );
});

test('npm package exposes both Catify command names', () => {
  const pkg = require('../../package.json');
  assert.equal(pkg.name, 'catify-cli');
  assert.equal(pkg.bin.cfy, 'bin/cfy.js');
  assert.equal(pkg.bin.catify, 'bin/cfy.js');
});

test('selects an exact archive checksum', () => {
  const digest = 'a'.repeat(64);
  const manifest = `${'b'.repeat(64)}  other.tar.gz\n${digest}  cfy.tar.gz\n`;
  assert.equal(checksumFromManifest(manifest, 'cfy.tar.gz'), digest);
  assert.throws(() => checksumFromManifest(manifest, 'missing.tar.gz'), /missing/);
});
