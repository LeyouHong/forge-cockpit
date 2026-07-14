/* ---------- boards (kanban) ---------- */
const KB_PLATFORMS = [
  { key: 'github', name: 'GitHub', logo: GH_LOGO },
  { key: 'gha', name: 'GitHub Actions', logo: GHA_LOGO },
  { key: 'jira', name: 'Jira', logo: JIRA_LOGO },
  { key: 'sentry', name: 'Sentry', logo: SENTRY_LOGO },
  { key: 'gcal', name: 'Google Calendar', logo: GCAL_LOGO },
  { key: 'slack', name: 'Slack', logo: SLACK_LOGO },
  { key: 'gmail', name: 'Gmail', logo: GMAIL_LOGO },
];
// openKanban lives in team.js, which loads after this file. Referencing it
// directly here would read it before its declaration is hoisted — one script's
// hoisting no longer covers the next. Called from inside the handler, the
// lookup happens at click time, by which point every script has run.
$('kanban-open').onclick = () => openKanban();
$('kanban-close').onclick = () => $('kanban-overlay').classList.remove('open');
$('kanban-refresh').onclick = () => openKanban();

/* ---------- integrations page ---------- */
function openIntegrations() { $('integrations-overlay').classList.add('open'); loadMcp(); }
$('integrations-open').onclick = openIntegrations;
$('integrations-close').onclick = () => $('integrations-overlay').classList.remove('open');
$('integrations-refresh').onclick = loadMcp;
