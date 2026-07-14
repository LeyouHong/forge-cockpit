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
// `openKanban` is a function declaration in team.js, which loads after this
// file. During boards.js execution it is still `undefined` in the global scope.
// That's fine: the click handler defers the lookup to click time, by which
// point every script has run and the function is available.
$('kanban-open').onclick = () => openKanban();
$('kanban-close').onclick = () => $('kanban-overlay').classList.remove('open');
$('kanban-refresh').onclick = () => openKanban();

/* ---------- integrations page ---------- */
function openIntegrations() { $('integrations-overlay').classList.add('open'); loadMcp(); }
$('integrations-open').onclick = openIntegrations;
$('integrations-close').onclick = () => $('integrations-overlay').classList.remove('open');
$('integrations-refresh').onclick = loadMcp;
