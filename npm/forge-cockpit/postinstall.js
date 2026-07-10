#!/usr/bin/env node
'use strict';

// Downloads the platform-specific `forge` binary from this version's GitHub
// Release into ./bin, at install time. Single npm package, no per-platform
// packages. Requires Node >=18 (uses global fetch, which follows redirects).

const fs = require('node:fs');
const path = require('node:path');
const { version } = require('./package.json');

const REPO = 'LeyouHong/forge-cockpit';

// host key -> release asset platform suffix
const PLATFORMS = {
  'darwin arm64': 'darwin-arm64',
  'linux x64': 'linux-x64',
  'linux arm64': 'linux-arm64',
  'win32 x64': 'win32-x64',
};

async function main() {
  const key = `${process.platform} ${process.arch}`;
  const suffix = PLATFORMS[key];
  if (!suffix) {
    console.warn(
      `forge-cockpit: no prebuilt binary for "${key}". ` +
        `Supported: ${Object.keys(PLATFORMS).join(', ')}. ` +
        `You can still build from source: https://github.com/${REPO}#build-from-source`
    );
    return; // don't hard-fail the install
  }

  const isWin = process.platform === 'win32';
  const ext = isWin ? '.exe' : '';
  const asset = `forge-cockpit-${suffix}${ext}`;
  const url = `https://github.com/${REPO}/releases/download/release-v${version}/${asset}`;

  const binDir = path.join(__dirname, 'bin');
  const dest = path.join(binDir, `forge${ext}`);
  fs.mkdirSync(binDir, { recursive: true });

  process.stdout.write(`forge-cockpit: downloading ${asset} (v${version})… `);
  let lastErr;
  for (let attempt = 1; attempt <= 3; attempt++) {
    try {
      const res = await fetch(url, { redirect: 'follow' });
      if (!res.ok) throw new Error(`HTTP ${res.status} for ${url}`);
      const buf = Buffer.from(await res.arrayBuffer());
      fs.writeFileSync(dest, buf);
      if (!isWin) fs.chmodSync(dest, 0o755);
      console.log('done.');
      return;
    } catch (e) {
      lastErr = e;
      if (attempt < 3) await new Promise((r) => setTimeout(r, 1500 * attempt));
    }
  }
  console.log('failed.');
  console.error(
    `forge-cockpit: could not download the binary (${lastErr && lastErr.message}).\n` +
      `Check your network, or install with scripts enabled (not --ignore-scripts).\n` +
      `Manual: download ${url} to ${dest}`
  );
  // Non-fatal: leave the launcher to report a clear error if run without a binary.
}

main().catch((e) => {
  console.error('forge-cockpit: postinstall error:', e && e.message ? e.message : e);
});
