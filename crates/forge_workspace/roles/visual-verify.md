## Visual verification (optional — degrade gracefully)

If the request produces a **renderable web UI** (an HTML page, a component, a
dashboard, a styling change), try to verify it *visually* — but this is an
enhancement, NOT a requirement. Never fail or block a request because a browser
isn't available.

**1. Detect a browser capability, in this order — use the first that works:**
   - a `playwright`-style MCP tool in your tool list (screenshot / navigate), or
   - `npx --yes playwright screenshot <url-or-file> <out.png>` (if `npx` works), or
   - headless Chrome: `"$(command -v google-chrome || command -v chromium ||
     echo '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome')"
     --headless=new --disable-gpu --screenshot=<out.png> <file-or-url>`

**2. If a browser IS available and this is a UI change:**
   - Render the target (open the built HTML file via `file://…`, or the dev
     server URL if one is running) and screenshot to
     `.forge-workspace/.team-shots/<request-id>.png`.
   - Check it renders without console/JS errors and that the DOM matches the
     acceptance criteria (right sections/controls present).
   - If your model can view images, `read` the screenshot and judge the visual
     result against the request (layout, spacing, contrast, responsiveness at a
     couple of widths). Report concrete visual findings.

**3. If NO browser is available, or this is not a UI change:**
   - Do a **code-level** UI review instead: structure/semantics, responsive
     breakpoints, accessibility, and consistency with the project's existing
     design system.
   - In your notes, state plainly: *"Visual verification skipped — no browser
     available; reviewed at the code level. To enable it, install Playwright
     (`npx playwright install`) or a browser."*

Be honest about which path you took. A missing browser lowers the verification
depth (code-only) — it must never turn into a failure or a fake "looks good".
