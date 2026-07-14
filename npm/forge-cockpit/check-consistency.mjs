// Guards the invariants that let a broken package ship before:
//
//   1. Every platform postinstall will try to download MUST be built by the
//      release workflow. A platform listed here with no matching build row is a
//      guaranteed 404 at install time (this is how Intel Mac broke: it was
//      missing from BOTH sides, so nobody noticed the pairing was the point).
//   2. Every file the package `require`s MUST be in package.json's `files`.
//      The repo's blanket `*.js` gitignore makes it easy to add a module that
//      is silently left out of the tarball and crashes on require() everywhere.
//
// Run: node npm/forge-cockpit/check-consistency.mjs

import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const require = createRequire(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');

const fail = [];

// --- 1. platform table vs the release build matrix -------------------------
const { PLATFORMS } = require(path.join(here, 'platforms.js'));
const workflow = readFileSync(path.join(repo, '.github/workflows/release.yml'), 'utf8');

const built = new Set([...workflow.matchAll(/^\s*-\s*\{[^}]*\bpkg:\s*([\w-]+)/gm)].map((m) => m[1]));
const wanted = new Set(Object.values(PLATFORMS));

for (const p of wanted) {
  if (!built.has(p)) {
    fail.push(`platforms.js offers "${p}" but release.yml builds no such asset — installs would 404.`);
  }
}
for (const p of built) {
  if (!wanted.has(p)) {
    fail.push(`release.yml builds "${p}" but platforms.js never offers it — the binary ships unused.`);
  }
}

// --- 2. every required module is actually packaged -------------------------
const pkg = JSON.parse(readFileSync(path.join(here, 'package.json'), 'utf8'));
const files = new Set(pkg.files ?? []);
for (const needed of ['platforms.js', 'postinstall.js', 'bin/forge-cockpit.js']) {
  if (!files.has(needed)) {
    fail.push(`package.json "files" is missing "${needed}" — it would not be published, and require() would throw for every user.`);
  }
}

if (fail.length) {
  console.error('npm package consistency check FAILED:\n');
  for (const f of fail) console.error(`  ✗ ${f}`);
  process.exit(1);
}

console.log(`ok — platforms match the build matrix (${[...wanted].sort().join(', ')}), and all required files are packaged.`);
