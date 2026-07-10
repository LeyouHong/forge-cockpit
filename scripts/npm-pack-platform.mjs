#!/usr/bin/env node
// Build a platform-specific npm package around a compiled `forge` binary.
//
// Usage:
//   node scripts/npm-pack-platform.mjs <pkgSuffix> <os> <cpu> <binaryPath> <version> [outDir]
//
// Example (in CI, per target):
//   node scripts/npm-pack-platform.mjs darwin-arm64 darwin arm64 \
//        target/aarch64-apple-darwin/release/forge 0.1.0 npm/dist
//
// Produces:  <outDir>/forge-cockpit-<pkgSuffix>/{package.json, README.md, bin/<exe>}
// which the release workflow then `npm publish`es.

import { chmodSync, copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { basename, join } from 'node:path';

const [, , suffix, os, cpu, binaryPath, version, outDir = 'npm/dist'] = process.argv;

if (!suffix || !os || !cpu || !binaryPath || !version) {
  console.error(
    'usage: npm-pack-platform.mjs <pkgSuffix> <os> <cpu> <binaryPath> <version> [outDir]'
  );
  process.exit(1);
}

const pkgName = `forge-cockpit-${suffix}`;
const exe = os === 'win32' ? 'forge.exe' : 'forge';
const pkgDir = join(outDir, pkgName);
const binDir = join(pkgDir, 'bin');
mkdirSync(binDir, { recursive: true });

// Copy the binary in as bin/forge(.exe) and make it executable.
copyFileSync(binaryPath, join(binDir, exe));
if (os !== 'win32') chmodSync(join(binDir, exe), 0o755);

const pkg = {
  name: pkgName,
  version,
  description: `forge-cockpit binary for ${os}-${cpu}`,
  license: 'Apache-2.0',
  repository: { type: 'git', url: 'github:LeyouHong/forge-cockpit' },
  os: [os],
  cpu: [cpu],
  // No `bin` field: the launcher resolves bin/<exe> directly via require.resolve.
  files: [`bin/${exe}`],
};
writeFileSync(join(pkgDir, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
writeFileSync(
  join(pkgDir, 'README.md'),
  `# ${pkgName}\n\nPlatform binary for [\`forge-cockpit\`](https://www.npmjs.com/package/forge-cockpit) ` +
    `on ${os}-${cpu}. Install \`forge-cockpit\` instead; npm picks this automatically.\n`
);

console.log(`packed ${pkgName} (${basename(binaryPath)} -> bin/${exe})`);
