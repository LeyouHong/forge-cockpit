#!/usr/bin/env node
'use strict';

// Downloads the platform-specific `forge` binary from this version's GitHub
// Release into ./bin, at install time. Single npm package, no per-platform
// packages. Requires Node >=18 (uses global fetch, which follows redirects).

const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { version } = require('./package.json');
const { REPO, BUILD_FROM_SOURCE, resolve } = require('./platforms');

async function main() {
  const host = resolve();

  // Unsupported host: fail the install. Returning quietly here left npm
  // reporting success and the CLI erroring at first run with a *misleading*
  // "install scripts were disabled" — sending users (notably Intel macOS and
  // Alpine) into a reinstall loop that could never work.
  if (!host.supported) {
    console.error(
      `forge-cockpit: ${host.reason}\n` +
        `forge-cockpit: build from source instead: ${BUILD_FROM_SOURCE}`
    );
    process.exit(1);
  }

  const isWin = process.platform === 'win32';
  const ext = isWin ? '.exe' : '';
  const asset = `forge-cockpit-${host.suffix}${ext}`;
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
      smokeTest(dest);
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

// Verify the downloaded binary actually runs on this system. Without this,
// install "succeeds" and the failure only surfaces at first run — e.g. a
// binary needing a newer glibc than the host provides (issue #10).
function smokeTest(bin) {
  try {
    execFileSync(bin, ['--version'], { stdio: 'pipe' });
  } catch (e) {
    const msg = String((e && e.stderr) || (e && e.message) || e);
    console.error(`forge-cockpit: the downloaded binary does not run on this system.\n${msg.trim()}`);
    if (msg.includes('GLIBC_')) {
      console.error(
        'forge-cockpit: your glibc is older than the binary requires (Linux builds need glibc >= 2.34, i.e. Ubuntu 22.04+ / Debian 12+).\n' +
          `forge-cockpit: you can build from source instead: ${BUILD_FROM_SOURCE}`
      );
    }
    fs.rmSync(bin, { force: true });
    process.exit(1); // fail the install: a "successful" install with an unusable CLI is worse
  }
}

main().catch((e) => {
  console.error('forge-cockpit: postinstall error:', e && e.message ? e.message : e);
});
