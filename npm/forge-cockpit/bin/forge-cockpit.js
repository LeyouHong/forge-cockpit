#!/usr/bin/env node
'use strict';

// Launcher for the `forge-cockpit` binary.
//
// The actual (Rust-compiled) binary ships in a platform-specific optional
// dependency — e.g. `forge-cockpit-darwin-arm64`. npm installs only the one
// matching the host (via each package's `os`/`cpu` fields), and this shim
// resolves it and execs it, forwarding all args and the exit code.

const { spawnSync } = require('node:child_process');

// host key -> platform package name
const PLATFORM_PACKAGES = {
  'darwin arm64': 'forge-cockpit-darwin-arm64',
  'linux x64': 'forge-cockpit-linux-x64',
  'linux arm64': 'forge-cockpit-linux-arm64',
  'win32 x64': 'forge-cockpit-win32-x64',
};

function resolveBinary() {
  const key = `${process.platform} ${process.arch}`;
  const pkg = PLATFORM_PACKAGES[key];
  if (!pkg) {
    throw new Error(
      `forge-cockpit: unsupported platform "${key}".\n` +
        `Supported: ${Object.values(PLATFORM_PACKAGES).join(', ')}`
    );
  }
  const exe = process.platform === 'win32' ? 'forge.exe' : 'forge';
  try {
    // Each platform package ships the binary at bin/<exe>.
    return require.resolve(`${pkg}/bin/${exe}`);
  } catch (_) {
    throw new Error(
      `forge-cockpit: the platform package "${pkg}" is not installed.\n` +
        `This usually means the optional dependency was skipped. Try:\n` +
        `  npm install --force forge-cockpit\n` +
        `or install the platform package directly:\n` +
        `  npm install ${pkg}`
    );
  }
}

function main() {
  let bin;
  try {
    bin = resolveBinary();
  } catch (err) {
    console.error(err.message);
    process.exit(1);
  }
  const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
  if (result.error) {
    console.error(`forge-cockpit: failed to launch binary: ${result.error.message}`);
    process.exit(1);
  }
  // Mirror the child's exit code (null => killed by signal).
  process.exit(result.status === null ? 1 : result.status);
}

main();
