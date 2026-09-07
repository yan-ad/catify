'use strict';

const https = require('node:https');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..');
const PACKAGE = require(path.join(ROOT, 'package.json'));
const REPOSITORY = process.env.CFY_REPOSITORY || 'yan-ad/catify';

function requestJson(url) {
  return new Promise((resolve, reject) => {
    const request = https.get(url, {
      headers: {
        accept: 'application/vnd.github+json',
        'user-agent': `catify-cli/${PACKAGE.version}`,
      },
    }, (response) => {
      const chunks = [];
      response.on('data', (chunk) => chunks.push(chunk));
      response.on('end', () => {
        const body = Buffer.concat(chunks).toString('utf8');
        if (response.statusCode !== 200) {
          reject(new Error(`GitHub release lookup failed (${response.statusCode}): ${body.slice(0, 200)}`));
          return;
        }
        try {
          resolve(JSON.parse(body));
        } catch (error) {
          reject(new Error(`GitHub release response was not valid JSON: ${error.message}`));
        }
      });
    });
    request.on('error', reject);
  });
}

function requiredAssets(version) {
  return [
    `cfy-v${version}-aarch64-apple-darwin.tar.gz`,
    `cfy-v${version}-x86_64-apple-darwin.tar.gz`,
    `cfy-v${version}-aarch64-unknown-linux-gnu.tar.gz`,
    `cfy-v${version}-x86_64-unknown-linux-gnu.tar.gz`,
    `cfy-v${version}-x86_64-pc-windows-msvc.zip`,
    'SHA256SUMS',
  ];
}

function validateRelease(release, version) {
  const names = new Set((release.assets || []).map((asset) => asset.name));
  const missing = requiredAssets(version).filter((name) => !names.has(name));
  if (missing.length > 0) {
    throw new Error(`GitHub release v${version} is missing assets: ${missing.join(', ')}`);
  }
}

async function main() {
  const version = PACKAGE.version;
  const release = await requestJson(`https://api.github.com/repos/${REPOSITORY}/releases/tags/v${version}`);
  validateRelease(release, version);
  console.log(`GitHub release v${version} contains all npm installer assets`);
}

if (require.main === module) {
  main().catch((error) => {
    console.error(`release asset preflight failed: ${error.message}`);
    process.exit(1);
  });
}

module.exports = { requiredAssets, validateRelease };
