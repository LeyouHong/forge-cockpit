# Changelog

All notable changes to `forge-cockpit` are documented here. This is a community
fork of Forge (Apache-2.0); versions are independent of upstream.

## 0.4.0

### Security
- **Content-Security-Policy on the cockpit.** The web UI now serves a strict CSP
  with a per-response nonce and no `unsafe-inline` for scripts. Text the cockpit
  renders from outside sources — GitHub issue titles, Sentry messages, Slack,
  Gmail, agent output — can no longer become script execution through an
  escaping slip.
- **Terminal token moved out of the URL.** The team terminal's access token now
  rides in the WebSocket subprotocol instead of the query string, so it no
  longer appears in devtools' network panel or proxy logs. The handshake never
  echoes it back.
- **Path-traversal fixed in the team code viewer.** `/api/team/file` and
  `/api/team/files` rejected `..` but not absolute paths, so a request like
  `path=/etc/passwd` could read any file the server user could. Absolute paths
  are now rejected too. (Token-gated and loopback-only, so not remotely
  reachable — this is defence-in-depth.)
- Constant-time comparison for the session token.

### Changed
- The Linux binaries require **glibc ≥ 2.34** (Ubuntu 22.04+ / Debian 12+),
  built on ubuntu-22.04. Installs on musl distros (Alpine) now **fail fast**
  with a clear message pointing at a glibc image or building from source, rather
  than downloading a binary that cannot run.
- Every release binary is now **run once on its own native platform** before
  publish (`--version`), and Linux builds assert their glibc ceiling — so a
  binary that installs but cannot start can no longer ship.

### Removed
- **Crafts** (AI-generated per-project mini-apps rendered in a sandboxed iframe).
  The feature saw no real use and has been removed end to end.

### Notes
- **Intel Mac (x86_64):** still no prebuilt binary — GitHub's last Intel CI
  runner (macos-13) is being retired and can no longer build the release.
  `npm install` fails fast with a clear message; build from source in the
  meantime. Apple Silicon (arm64) is unaffected.

### Internal
- The cockpit's 3926-line single-file frontend was split into ten scripts by
  concern; no behaviour change (verified in a browser before and after).
- CI now verifies the npm package itself — the platform table matches the build
  matrix, and every runtime file is in the published tarball.
- Poisoned-mutex recovery in the web server: one panicking request can no longer
  cascade into every later request panicking.

## 0.3.0

- Terminal-resident team, message bus, watches, web terminal, and the initial
  glibc build fix. (First release of this line.)
