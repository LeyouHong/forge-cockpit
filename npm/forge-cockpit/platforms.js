'use strict';

// Single source of truth for which hosts have a prebuilt binary. Shared by
// postinstall.js (what to download) and bin/forge-cockpit.js (what to say when
// the binary isn't there). Keep in sync with the build matrix in
// .github/workflows/release.yml — a host listed here MUST have a release asset,
// or install will fail with a 404.

const REPO = 'LeyouHong/forge-cockpit';
const BUILD_FROM_SOURCE = `https://github.com/${REPO}#build-from-source`;

// `${process.platform} ${process.arch}` -> release asset platform suffix
const PLATFORMS = {
  'darwin arm64': 'darwin-arm64',
  // No 'darwin x64': GitHub's Intel-Mac runner (macos-13) is being retired, so
  // release CI can't build it. Intel Macs fall back to build-from-source rather
  // than 404 on a binary that was never published. Keep in sync with the build
  // matrix in .github/workflows/release.yml (check-consistency.mjs enforces it).
  'linux x64': 'linux-x64',
  'linux arm64': 'linux-arm64',
  'win32 x64': 'win32-x64',
};

// The Linux binaries are glibc-linked (built on ubuntu-22.04, glibc >= 2.34).
// On a musl distro (Alpine) they download fine and then die in the loader, so
// detect musl up front and say so — otherwise the user just sees the smoke test
// report a cryptic "cannot execute" with no hint about why.
function isMusl() {
  if (process.platform !== 'linux') return false;
  try {
    // Non-empty on glibc; absent/empty on musl.
    const report = process.report.getReport();
    return !(report && report.header && report.header.glibcVersionRuntime);
  } catch {
    return false; // can't tell — assume glibc and let the smoke test catch it
  }
}

// -> { supported: true, key, suffix }
//  | { supported: false, key, reason }
function resolve() {
  const key = `${process.platform} ${process.arch}`;

  if (isMusl()) {
    return {
      supported: false,
      key,
      reason:
        'musl-based Linux (Alpine) is not supported: the prebuilt Linux binaries are ' +
        'glibc-linked (glibc >= 2.34). Use a glibc image (e.g. node:20-slim, ' +
        'debian, ubuntu) or build from source.',
    };
  }

  const suffix = PLATFORMS[key];
  if (!suffix) {
    return {
      supported: false,
      key,
      reason:
        `no prebuilt binary for "${key}". ` +
        `Supported: ${Object.keys(PLATFORMS).join(', ')}.`,
    };
  }

  return { supported: true, key, suffix };
}

module.exports = { REPO, BUILD_FROM_SOURCE, PLATFORMS, resolve, isMusl };
