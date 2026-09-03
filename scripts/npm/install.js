'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { fileURLToPath } = require('node:url');

const ROOT = path.resolve(__dirname, '..', '..');
const PACKAGE = require(path.join(ROOT, 'package.json'));
const DEFAULT_RELEASE_BASE = 'https://github.com/yan-ad/catify/releases/download';

function targetFor(platform = process.platform, arch = process.arch) {
  const targets = {
    'darwin-arm64': 'aarch64-apple-darwin',
    'darwin-x64': 'x86_64-apple-darwin',
    'linux-arm64': 'aarch64-unknown-linux-gnu',
    'linux-x64': 'x86_64-unknown-linux-gnu',
    'win32-x64': 'x86_64-pc-windows-msvc',
  };
  const target = targets[`${platform}-${arch}`];
  if (!target) {
    throw new Error(`unsupported platform: ${platform}/${arch}`);
  }
  return target;
}

function checksumFromManifest(manifest, archiveName) {
  for (const line of manifest.split(/\r?\n/)) {
    const match = line.match(/^([a-fA-F0-9]{64})\s+\*?(.+)$/);
    if (match && match[2] === archiveName) return match[1].toLowerCase();
  }
  throw new Error(`${archiveName} is missing from SHA256SUMS`);
}

function download(url, redirects = 5) {
  const parsed = new URL(url);
  if (parsed.protocol === 'file:') {
    return fs.promises.readFile(fileURLToPath(parsed));
  }
  if (redirects < 0) return Promise.reject(new Error(`too many redirects for ${url}`));
  const client = parsed.protocol === 'http:' ? http : https;
  return new Promise((resolve, reject) => {
    const request = client.get(parsed, {
      headers: { 'user-agent': `catify-cli/${PACKAGE.version}` },
    }, (response) => {
      if (response.statusCode >= 300 && response.statusCode < 400 && response.headers.location) {
        response.resume();
        resolve(download(new URL(response.headers.location, parsed).toString(), redirects - 1));
        return;
      }
      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`download failed (${response.statusCode}) for ${url}`));
        return;
      }
      const chunks = [];
      response.on('data', (chunk) => chunks.push(chunk));
      response.on('end', () => resolve(Buffer.concat(chunks)));
    });
    request.on('error', reject);
  });
}

function extract(archive, destination, platform = process.platform) {
  let command;
  let args;
  let env = process.env;
  if (platform === 'win32') {
    command = 'powershell.exe';
    args = [
      '-NoLogo',
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      'Expand-Archive -LiteralPath $env:CFY_ARCHIVE -DestinationPath $env:CFY_DESTINATION -Force',
    ];
    env = { ...process.env, CFY_ARCHIVE: archive, CFY_DESTINATION: destination };
  } else {
    command = 'tar';
    args = ['-xzf', archive, '-C', destination];
  }
  const result = spawnSync(command, args, { stdio: 'inherit', env });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with status ${result.status}`);
}

async function install() {
  if (process.env.CFY_SKIP_DOWNLOAD === '1') return;

  const version = PACKAGE.version;
  const target = targetFor();
  const extension = process.platform === 'win32' ? '.zip' : '.tar.gz';
  const archiveName = `cfy-v${version}-${target}${extension}`;
  const releaseBase = (process.env.CFY_RELEASE_BASE_URL || DEFAULT_RELEASE_BASE).replace(/\/$/, '');
  const assetBase = `${releaseBase}/v${version}`;
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'catify-install-'));

  try {
    const [manifest, archive] = await Promise.all([
      download(`${assetBase}/SHA256SUMS`),
      download(`${assetBase}/${archiveName}`),
    ]);
    const expected = checksumFromManifest(manifest.toString('utf8'), archiveName);
    const actual = crypto.createHash('sha256').update(archive).digest('hex');
    if (actual !== expected) {
      throw new Error(`checksum mismatch for ${archiveName}: expected ${expected}, got ${actual}`);
    }

    const archivePath = path.join(temp, archiveName);
    fs.writeFileSync(archivePath, archive);
    extract(archivePath, temp);

    const binaryName = process.platform === 'win32' ? 'cfy.exe' : 'cfy';
    const source = path.join(temp, `cfy-v${version}-${target}`, binaryName);
    if (!fs.statSync(source).isFile()) throw new Error(`release archive has no ${binaryName}`);

    const vendor = path.join(ROOT, 'vendor');
    const destination = path.join(vendor, binaryName);
    const staging = `${destination}.tmp-${process.pid}`;
    fs.mkdirSync(vendor, { recursive: true });
    fs.copyFileSync(source, staging);
    if (process.platform !== 'win32') fs.chmodSync(staging, 0o755);
    fs.renameSync(staging, destination);
    console.log(`Installed Catify ${version} for ${target}`);
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
}

if (require.main === module) {
  install().catch((error) => {
    console.error(`catify-cli installation failed: ${error.message}`);
    process.exit(1);
  });
}

module.exports = { checksumFromManifest, download, extract, install, targetFor };
