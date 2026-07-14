#!/usr/bin/env node
'use strict';

// Launcher: exec the `forge` binary that postinstall.js downloaded next to this
// file (bin/forge or bin/forge.exe), forwarding all args and the exit code.

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const { BUILD_FROM_SOURCE, resolve } = require('../platforms');

const exe = process.platform === 'win32' ? 'forge.exe' : 'forge';
const bin = path.join(__dirname, exe);

if (!fs.existsSync(bin)) {
  // Two very different causes, and telling them apart matters: advising someone
  // on an unsupported host to "reinstall with scripts enabled" is a loop that
  // can never succeed. Name the real reason.
  const host = resolve();
  if (!host.supported) {
    console.error(
      `forge-cockpit: ${host.reason}\n` +
        `forge-cockpit: build from source: ${BUILD_FROM_SOURCE}`
    );
  } else {
    console.error(
      'forge-cockpit: the binary was not downloaded.\n' +
        'This usually means install scripts were disabled. Reinstall with scripts enabled:\n' +
        '  npm install -g forge-cockpit\n' +
        `or build from source: ${BUILD_FROM_SOURCE}`
    );
  }
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  console.error(`forge-cockpit: failed to launch binary: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
