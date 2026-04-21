'use strict';

// ── Tauri bridge ─────────────────────────────────────────────────────────────
// window.__TAURI__ is injected when running inside Tauri (withGlobalTauri: true).
// Tauri 2.x exposes: window.__TAURI__.core.invoke  and  window.__TAURI__.event.listen
// Wrap defensively so a bad path never crashes the entire script block.
const _T = (typeof window.__TAURI__ !== 'undefined') ? window.__TAURI__ : null;
const invoke = (_T && _T.core && typeof _T.core.invoke === 'function')
  ? (...a) => _T.core.invoke(...a)
  : async (cmd, args) => { console.warn('[invoke stub]', cmd, args); return null; };
const listen = (_T && _T.event && typeof _T.event.listen === 'function')
  ? (...a) => _T.event.listen(...a)
  : async () => () => {};
const _isTauri = !!(_T && _T.core);

// ── App state ─────────────────────────────────────────────────────────────────
let cfg = {
  token:         '',   // team token (tm_…) — what workers auth with
  teamName:      '',   // team.yml `team:` key
  serverUrl:     'http://localhost:8000',
  identity:      'human',
  projectDir:    '',
  setupComplete: false,
  cliTemplate:   'claude -p {prompt} --model {model} --allowedTools Bash,Read,Write,Edit',
  model:         'haiku',
};

let workers = [
  { name: 'builder',  role: 'Build features and write code' },
  { name: 'reviewer', role: 'Review code and provide feedback' },
];

let currentStep = 1;
let sseConn     = null;
let sseRetries  = 0;
let rosterTimer = null;
let todosTimer  = null;
let serverPollTimer = null;
let presenceTimer = null;
let dashboardActive = false;
let wizardFromDashboard = false;

// Message state
let allMessages = [];      // [{id,sender,recipient,content,timestamp,hash}]
let senderColors = {};     // sender → color index 0-5
let colorCounter = 0;
let activeTab = 'all';
let unreadMentions = 0;
let todosVisible  = false;
let serverLogOpen = false;
let usageOpen     = false;
let usageTimer    = null;
let knownWorkers  = [];   // [{instance_id, role}] — updated when roster fetched

const CLI_TEMPLATES = {
  claude:  'claude -p {prompt} --model {model} --allowedTools Bash,Read,Write,Edit',
  cursor:  'cursor -p {prompt}',
  ollama:  'ollama run llama3.1 {prompt}',
  codex:   'codex -p {prompt} --model {model}',
  custom:  '',
};

// ── Init ──────────────────────────────────────────────────────────────────────
(async function init() {
  // Load persisted config
  try {
    const saved = await invoke('load_config');
    if (saved) Object.assign(cfg, saved);
  } catch (e) { /* not in Tauri */ }

  // Listen for server log events
  await listen('server-log', e => appendServerLog(e.payload, false));
  // Listen for command output during launch
  await listen('cmd-output', e => {
    const p = e.payload;
    appendLaunchLog(p.line, p.stream === 'err');
    appendServerLog(p.line, p.stream === 'err');
  });

  // teamName is required by the new schema — treat its absence as "config
  // was saved by a pre-team version of the wizard" and drop back into Step 1
  // so the human can mint a team token instead of auto-booting into a
  // dashboard the backend no longer understands.
  if (cfg.setupComplete && cfg.token && cfg.teamName) {
    // Already set up — hide wizard, show dashboard, and restart the server.
    // On a fresh app launch the server sidecar isn't running yet, so SSE
    // would loop on "reconnecting…" forever without this.
    const wiz  = document.getElementById('wizard');
    const dash = document.getElementById('dashboard');
    wiz.hidden = true;
    dash.hidden = false;
    requestAnimationFrame(() => dash.classList.add('visible'));
    dashboardActive = true;
    showDashboard();
    // Register the session FIRST so the Cmd+Q / close-window handler will
    // warn about still-running workers even if start_server below races
    // or errors out. Without this, a quick quit after the app reopens
    // (before the server sidecar has finished starting) leaves worker
    // daemons running silently with no prompt — which burns tokens. Must
    // be awaited, not fire-and-forget.
    try { await invoke('mark_session_active', { projectDir: cfg.projectDir }); } catch {}

    // Start server in background — connectSSE (called by showDashboard)
    // will keep retrying until the server is ready. Admin token is
    // generated fresh per session; the team token already lives in
    // team_tokens on the server disk, so workers auth with cfg.token
    // via their own envs, not via this startup parameter.
    invoke('start_server', {
      serverUrl:  cfg.serverUrl,
      adminToken: generateHexToken(32),
      projectDir: cfg.projectDir,
    }).catch(e => toast('Server error: ' + e, true));
  } else {
    // Show wizard, pre-fill fields
    prefillWizard();
    // If a project dir is already remembered and has workers.yaml, load it.
    if (cfg.projectDir) {
      try { await loadExistingProject(cfg.projectDir); } catch (e) {}
    }
  }
})();

// ── Wizard helpers ────────────────────────────────────────────────────────────
function prefillWizard() {
  if (cfg.token)       document.getElementById('s1-token').value       = cfg.token;
  if (cfg.teamName)    document.getElementById('s1-team-name').value   = cfg.teamName;
  if (cfg.serverUrl)   document.getElementById('s1-url').value         = cfg.serverUrl;
  if (cfg.identity)    document.getElementById('s1-identity').value    = cfg.identity;
  if (cfg.projectDir)  document.getElementById('s2-dir').value         = cfg.projectDir;
  renderWorkerCards();
}

function goStep(n) {
  const prev = currentStep;
  currentStep = n;

  document.getElementById(`step-${prev}`).className = `step ${n > prev ? 'past' : 'future'}`;
  document.getElementById(`step-${n}`).className    = 'step active';

  // Progress dots
  document.querySelectorAll('.wiz-dot').forEach((d, i) => {
    const s = i + 1;
    d.className = 'wiz-dot ' + (s < n ? 'done' : s === n ? 'active' : '');
  });
}

function updateWizardStep1Nav() {
  const step1Nav = document.querySelector('#step-1 .step-nav');
  if (!step1Nav) return;

  // Clear existing back button if any
  const existingBack = step1Nav.querySelector('#back-from-step1');
  if (existingBack) existingBack.remove();

  // Add back button if coming from dashboard
  if (wizardFromDashboard) {
    const backBtn = document.createElement('button');
    backBtn.className = 'btn btn-ghost';
    backBtn.textContent = '← Back';
    backBtn.id = 'back-from-step1';
    backBtn.addEventListener('click', backFromWizard);
    step1Nav.insertBefore(backBtn, step1Nav.firstChild);
  }
}

function backFromWizard() {
  if (wizardFromDashboard) {
    wizardFromDashboard = false;
    dashboardActive = true;
    const wiz  = document.getElementById('wizard');
    const dash = document.getElementById('dashboard');
    wiz.classList.add('exiting');
    setTimeout(() => {
      wiz.hidden = true;
      dash.hidden = false;
      dash.classList.add('visible');
    }, 350);
  }
}


// Matches the server's is_valid_team_name: alphanumeric + `-`/`_`, 1–64 chars.
// Letters are case-preserved — there's no reason to force lowercase here
// when "D4LFG" or "BlenderRig" are perfectly valid team names upstream.
const TEAM_NAME_RE = /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/;

// Step 1 validation. Requires URL + team name + chat identity. The team
// token is optional — if empty, the wizard mints one against the live
// server during Step 4 launch (no admin token needed when the server
// runs with no COLLAB_TOKEN configured, which is the single-user case).
function step1Next() {
  const token    = document.getElementById('s1-token').value.trim();
  const teamName = document.getElementById('s1-team-name').value.trim();
  const url      = document.getElementById('s1-url').value.trim();
  const identity = document.getElementById('s1-identity').value.trim();

  if (!url)      { toast('Enter the server URL.', true); return; }
  if (!teamName) { toast('Name this team.', true); return; }
  if (!TEAM_NAME_RE.test(teamName)) {
    toast('Team name: letters, numbers, dash, underscore. 1–64 chars.', true);
    return;
  }
  if (!identity) { toast('Enter your name for the chat.', true); return; }

  cfg.token     = token;          // may be empty — filled at launch if so
  cfg.teamName  = teamName;
  cfg.serverUrl = url;
  cfg.identity  = identity;
  goStep(2);
}

// Mint a fresh 64-char hex secret into the Team Token field and reveal it
// briefly so the user can eyeball / copy it before the field locks back to
// password display. Mirrors the Paste button's 3-second reveal so users
// aren't surprised by the type flip.
function doGenerateTeamToken() {
  const input = document.getElementById('s1-token');
  if (!input) return;
  input.value = generateHexToken(32);
  input.type = 'text';
  setTimeout(() => { input.type = 'password'; }, 3000);
}

// Mint a team token against the currently-running server. Called from
// doLaunch after start_server, so there IS a server to talk to. The
// Bearer token is the per-session admin secret the GUI generated when
// spawning the server — server requires an admin token on /admin/teams
// and this is the only one that exists for this session.
async function mintTeamTokenDuringLaunch(adminToken) {
  const url = cfg.serverUrl.replace(/\/+$/, '') + '/admin/teams';
  const resp = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'Authorization': `Bearer ${adminToken}`,
    },
    body: JSON.stringify({ name: cfg.teamName }),
  });
  if (resp.status === 409) {
    throw new Error(
      `A team named "${cfg.teamName}" already exists on this server. ` +
      `Go back to Step 1, open "I already have a team token", and paste its tm_… token.`
    );
  }
  if (!resp.ok) {
    const body = await resp.text().catch(() => '');
    throw new Error(`Server rejected team mint (HTTP ${resp.status}): ${body.slice(0, 200)}`);
  }
  const data = await resp.json();
  if (!data.token) throw new Error('Server minted the team but returned no token.');
  return data.token;
}

// Step 2 validation
function step2Next() {
  const dir = document.getElementById('s2-dir').value.trim();
  if (!dir) { toast('Choose a project directory first.', true); return; }
  cfg.projectDir = dir;
  goStep(3);
}

// Step 3 validation
function step3Next() {
  // Pull values from worker cards
  const cards = document.querySelectorAll('.worker-card');
  const updated = [];
  let ok = true;
  cards.forEach(card => {
    const name = card.querySelector('.wc-name').value.trim();
    const role = card.querySelector('.wc-role').value.trim();
    if (!name) { toast('All workers need a name.', true); ok = false; }
    if (!role) { toast('All workers need a role description.', true); ok = false; }
    if (!/^[a-z0-9][a-z0-9-]*$/.test(name)) {
      toast(`Worker name "${name}" must be lowercase letters, numbers, and hyphens only.`, true);
      ok = false;
    }
    // Preserve any extra fields (model, cli_template, etc.) from the existing worker
    const existing = workers[Array.from(cards).indexOf(card)] || {};
    const modelEl = card.querySelector('.wc-model');
    const tmplEl  = card.querySelector('.wc-cli-template');
    const cbEl    = card.querySelector('.wc-codebase');
    const entry = { ...existing, name, role };
    if (modelEl) entry.model = modelEl.value.trim() || undefined;
    if (tmplEl)  entry.cli_template = tmplEl.value.trim() || undefined;
    if (cbEl)    entry.codebase_path = cbEl.value.trim() || undefined;
    updated.push(entry);
  });
  if (!ok) return;
  if (updated.length === 0) { toast('Add at least one worker.', true); return; }
  workers = updated;
  cfg.cliTemplate = getCliTemplate();
  cfg.model       = document.getElementById('s3-model').value.trim() || 'haiku';
  goStep(4);
}

function getCliTemplate() {
  const tool = document.getElementById('s3-tool').value;
  if (tool === 'custom') return document.getElementById('s3-custom-tmpl').value.trim();
  return CLI_TEMPLATES[tool] || CLI_TEMPLATES.claude;
}

function onToolChange() {
  const tool = document.getElementById('s3-tool').value;
  document.getElementById('custom-template-field').hidden = (tool !== 'custom');
}

// Worker card rendering
function renderWorkerCards() {
  const list = document.getElementById('workers-list');
  list.innerHTML = '';
  workers.forEach((w, i) => {
    const div = document.createElement('div');
    div.className = 'worker-card';
    const hasAdvanced = w.model || w.cli_template || w.codebase_path;
    div.innerHTML = `
      <div class="wc-top">
        <div class="field">
          <label>Name</label>
          <input class="inp wc-name inp-mono" type="text" value="${esc(w.name)}" placeholder="backend" autocomplete="off" spellcheck="false">
        </div>
        <div class="field">
          <label>Role</label>
          <input class="inp wc-role" type="text" value="${esc(w.role)}" placeholder="What this worker does">
        </div>
      </div>
      <div class="wc-advanced-wrap">
        <button type="button" class="btn btn-ghost btn-xs wc-adv-toggle">${hasAdvanced ? '▾' : '▸'} Advanced</button>
        <div class="wc-advanced" ${hasAdvanced ? '' : 'hidden'}>
          <div class="field">
            <label>Codebase path override</label>
            <div class="dir-row">
              <input class="inp wc-codebase inp-mono" type="text" value="${esc(w.codebase_path || '')}" placeholder="(use project folder from Step 2)">
              <button type="button" class="btn btn-ghost wc-browse">Browse</button>
            </div>
            <div class="hint-box">Set this when a worker lives in a different repo than the rest of the team.</div>
          </div>
          <div class="field">
            <label>Model override</label>
            <input class="inp wc-model inp-mono" type="text" value="${esc(w.model || '')}" placeholder="(use default)">
          </div>
          <div class="field">
            <label>CLI template override</label>
            <input class="inp wc-cli-template inp-mono" type="text" value="${esc(w.cli_template || '')}" placeholder="(use default)">
          </div>
        </div>
      </div>
    `;
    div.querySelector('.wc-adv-toggle').addEventListener('click', e => {
      const panel = div.querySelector('.wc-advanced');
      panel.hidden = !panel.hidden;
      e.target.textContent = (panel.hidden ? '▸' : '▾') + ' Advanced';
    });
    div.querySelector('.wc-browse').addEventListener('click', async () => {
      try {
        const dir = await invoke('pick_directory');
        if (!dir) return;
        const cbInput = div.querySelector('.wc-codebase');
        cbInput.value = dir;
        // Persist immediately so re-renders (e.g. from addWorker elsewhere)
        // don't clobber the pick.
        syncWorkersFromDom();
      } catch (e) {
        toast('Could not open directory picker: ' + e, true);
      }
    });
    const rm = document.createElement('button');
    rm.className = 'btn btn-ghost btn-icon wc-remove';
    rm.title = 'Remove worker';
    rm.textContent = '✕';
    rm.addEventListener('click', () => removeWorker(i));
    div.appendChild(rm);
    list.appendChild(div);
  });
}

// Read the current on-screen worker card values back into `workers` so that
// a subsequent re-render doesn't clobber edits the user has typed but not yet
// submitted. Any card whose DOM is gone is skipped.
function syncWorkersFromDom() {
  const cards = document.querySelectorAll('.worker-card');
  if (!cards.length) return;
  const updated = [];
  cards.forEach((card, i) => {
    const nameEl = card.querySelector('.wc-name');
    const roleEl = card.querySelector('.wc-role');
    const modelEl = card.querySelector('.wc-model');
    const tmplEl  = card.querySelector('.wc-cli-template');
    const cbEl    = card.querySelector('.wc-codebase');
    const existing = workers[i] || {};
    const entry = {
      ...existing,
      name: nameEl ? nameEl.value : '',
      role: roleEl ? roleEl.value : '',
    };
    if (modelEl) entry.model = modelEl.value.trim() || undefined;
    if (tmplEl)  entry.cli_template = tmplEl.value.trim() || undefined;
    if (cbEl)    entry.codebase_path = cbEl.value.trim() || undefined;
    updated.push(entry);
  });
  workers = updated;
}

function addWorker() {
  syncWorkersFromDom();
  workers.push({ name: '', role: '' });
  renderWorkerCards();
}

function removeWorker(i) {
  syncWorkersFromDom();
  workers.splice(i, 1);
  renderWorkerCards();
}

function _diagBanner(msg, kind) {
  let el = document.getElementById('diag-banner');
  if (!el) {
    el = document.createElement('div');
    el.id = 'diag-banner';
    document.body.appendChild(el);
  }
  el.className = 'diag-banner' + (kind === 'err' ? ' err' : kind === 'ok' ? ' ok' : '');
  el.textContent = msg;
}

// 32-byte hex token. Used for the per-session admin secret the GUI hands
// the collab-server sidecar at startup; never persisted, never shown.
function generateHexToken(bytes = 32) {
  const buf = new Uint8Array(bytes);
  crypto.getRandomValues(buf);
  return Array.from(buf, b => b.toString(16).padStart(2, '0')).join('');
}

// Convenience: read a team token from the clipboard. Saves the user a
// paste-into-masked-field step, and flips the field visible briefly so
// they can verify they got the right one.
async function doPasteTeamToken() {
  const input = document.getElementById('s1-token');
  if (!input) return;
  try {
    const text = (await navigator.clipboard.readText()).trim();
    if (!text) { toast('Clipboard is empty.', true); return; }
    input.value = text;
    input.type = 'text';
    setTimeout(() => { input.type = 'password'; }, 3000);
  } catch (e) {
    toast('Could not read clipboard — paste manually into the field.', true);
  }
}

window.addEventListener('error', (e) => {
  _diagBanner('UNCAUGHT: ' + (e && e.message ? e.message : 'unknown') + ' @ ' + (e.filename||'?') + ':' + (e.lineno||'?'), 'err');
});
window.addEventListener('unhandledrejection', (e) => {
  _diagBanner('UNHANDLED REJECTION: ' + (e && e.reason ? e.reason : 'unknown'), 'err');
});

async function doBrowse() {
  try {
    const dir = await invoke('pick_directory');
    if (dir) {
      document.getElementById('s2-dir').value = dir;
      cfg.projectDir = dir;
      await loadExistingProject(dir);
    }
  } catch (e) {
    toast('Could not open directory picker: ' + e, true);
  }
}

// If <dir>/team.yml (preferred) or <dir>/workers.yaml exists, parse it and
// populate the wizard state so the user sees their existing project
// instead of the blank defaults. team.yml wins — workers.yaml is only
// read as a legacy fallback for projects that predate the team refactor.
async function loadExistingProject(dir) {
  const teamPath    = dir + '/team.yml';
  const legacyPath  = dir + '/workers.yaml';
  let yamlPath = null;
  let sourceLabel = null;
  try {
    if (await invoke('path_exists', { path: teamPath }))   { yamlPath = teamPath;   sourceLabel = 'team.yml'; }
    else if (await invoke('path_exists', { path: legacyPath })) { yamlPath = legacyPath; sourceLabel = 'workers.yaml'; }
  } catch (e) {}
  if (!yamlPath) return false;

  let text = '';
  try { text = await invoke('read_file', { path: yamlPath }); }
  catch (e) { toast(`Found ${sourceLabel} but could not read it: ` + e, true); return false; }

  const parsed = parseTeamOrWorkersYaml(text);
  if (!parsed) return false;

  if (parsed.team)         { cfg.teamName = parsed.team; const el = document.getElementById('s1-team-name'); if (el) el.value = parsed.team; }
  if (parsed.cli_template) cfg.cliTemplate = parsed.cli_template;
  if (parsed.model)        cfg.model       = parsed.model;
  if (parsed.workers && parsed.workers.length) workers = parsed.workers;

  // Reflect into the step-3 controls if they're mounted.
  const toolEl = document.getElementById('s3-tool');
  if (toolEl) {
    const match = Object.entries(CLI_TEMPLATES).find(([k, v]) => v && v === cfg.cliTemplate);
    if (match) {
      toolEl.value = match[0];
    } else {
      toolEl.value = 'custom';
      const tmplEl = document.getElementById('s3-custom-tmpl');
      if (tmplEl) tmplEl.value = cfg.cliTemplate;
    }
    onToolChange();
  }
  const modelEl = document.getElementById('s3-model');
  if (modelEl && cfg.model) modelEl.value = cfg.model;
  renderWorkerCards();

  toast(`Loaded existing ${sourceLabel} from ${dir}`);
  return true;
}

// Minimal parser for the exact shape buildTeamYaml (and legacy
// buildWorkersYaml) produces: flat scalar keys plus a `workers:` list of
// `- name: x` / `  role: "y"` / `  codebase_path: "z"` rows. Not a
// general YAML parser — anything fancier is ignored.
function parseTeamOrWorkersYaml(text) {
  const out = { workers: [] };
  const lines = text.split(/\r?\n/);
  const unquote = s => {
    s = s.trim();
    if ((s.startsWith('"') && s.endsWith('"')) || (s.startsWith("'") && s.endsWith("'"))) {
      return s.slice(1, -1).replace(/\\"/g, '"');
    }
    return s;
  };
  let inWorkers = false;
  let current = null;
  for (const raw of lines) {
    if (!raw.trim() || raw.trim().startsWith('#')) continue;
    if (!inWorkers) {
      if (/^workers\s*:\s*$/.test(raw)) { inWorkers = true; continue; }
      const m = raw.match(/^([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.*)$/);
      if (m) out[m[1]] = unquote(m[2]);
      continue;
    }
    // Inside `workers:` block
    const item = raw.match(/^\s*-\s*name\s*:\s*(.*)$/);
    if (item) {
      if (current) out.workers.push(current);
      current = { name: unquote(item[1]), role: '' };
      continue;
    }
    const field = raw.match(/^\s+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.*)$/);
    if (field && current) {
      current[field[1]] = unquote(field[2]);
      continue;
    }
    // A top-level key after the list ends the block.
    if (/^[A-Za-z_]/.test(raw)) {
      if (current) { out.workers.push(current); current = null; }
      inWorkers = false;
      const m = raw.match(/^([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.*)$/);
      if (m) out[m[1]] = unquote(m[2]);
    }
  }
  if (current) out.workers.push(current);
  return out;
}

// ── Launch sequence ───────────────────────────────────────────────────────────
async function doLaunch() {
  document.getElementById('launch-btn').hidden = true;
  document.getElementById('back-from-4').disabled = true;
  clearLaunchLog();
  // Track non-fatal failures so we can hold the user on this screen instead
  // of auto-jumping to the dashboard before they can read the error.
  let launchHadError = false;

  // Workers auth with the team token via COLLAB_TOKEN. If we don't have
  // one yet (Step 1 left it blank), we mint one after the server starts
  // — see the `if (!cfg.token)` block further down — then patch envs in
  // place before subsequent collab invocations inherit them.
  const envs = [
    ['COLLAB_TOKEN', cfg.token],
    ['COLLAB_SERVER', cfg.serverUrl],
    ['COLLAB_INSTANCE', cfg.identity || 'gui'],
  ];
  const setEnv = (k, v) => {
    const row = envs.find(e => e[0] === k);
    if (row) row[1] = v; else envs.push([k, v]);
  };

  // Per-launch admin secret. The server refuses to start without an admin
  // token, and we need to be the one that holds it so we can hit
  // /admin/teams during the mint step. Never saved — regenerated every
  // launch. Equivalent to the human typing `COLLAB_ADMIN_TOKEN=...
  // collab-server` in a terminal, except the human doesn't have to know
  // about any of it.
  const sessionAdminToken = generateHexToken(32);

  // Step 1: Write team.yml. Skip the write when the current wizard state
  // exactly matches what's on disk (so a re-launch is a no-op), but
  // otherwise always write — the user went through the wizard on purpose.
  setLaunchItem('li-config', 'running');
  const yaml = buildTeamYaml();
  const yamlPath = cfg.projectDir + '/team.yml';
  let existing = null;
  try {
    const exists = await invoke('path_exists', { path: yamlPath });
    if (exists) existing = await invoke('read_file', { path: yamlPath });
  } catch (e) { /* treat as missing */ }

  if (existing === yaml) {
    setLaunchItem('li-config', 'done');
    appendLaunchLog('• team.yml unchanged, skipping write', false);
  } else {
    appendLaunchLog((existing === null ? 'Writing ' : 'Updating ') + yamlPath, false);
    try {
      await invoke('write_file', { path: yamlPath, content: yaml });
      setLaunchItem('li-config', 'done');
      appendLaunchLog(existing === null ? '✓ team.yml written' : '✓ team.yml updated', false);
    } catch (e) {
      setLaunchItem('li-config', 'error', e);
      appendLaunchLog('✗ ' + e, true);
      resetLaunchBtn();
      return;
    }
  }

  // Step 2: Start collab-server — unless one is already reachable at the
  // configured URL. A reachable server means the user is joining an existing
  // team (e.g. a mac worker connecting to a Windows host over Tailscale) or
  // simply re-launching against a server that's already up. Either way,
  // spawning a new one on top would fight for the port and break auth.
  //
  // Probe bearer: use the team token if we have one (re-launch / joiner
  // case) or the fresh session admin token if we don't (first-launch
  // case — an existing server would reject this, correctly telling us
  // there's a conflict we need to surface).
  setLaunchItem('li-server', 'running');
  let serverAlreadyRunning = false;
  try {
    const probeUrl = cfg.serverUrl.replace(/\/+$/, '') + '/';
    const probeBearer = cfg.token || sessionAdminToken;
    const probe = await fetch(probeUrl, {
      headers: { Authorization: `Bearer ${probeBearer}` },
      signal: AbortSignal.timeout(2500),
    });
    if (probe.status === 200) {
      serverAlreadyRunning = true;
      appendLaunchLog('✓ Found existing server at ' + cfg.serverUrl + ' — skipping local spawn', false);
      // Register the session anyway so Cmd+Q / close-window warns about
      // the local worker daemons this GUI is about to start, even though
      // we skipped start_server (which normally records this).
      try {
        await invoke('mark_session_active', { projectDir: cfg.projectDir });
      } catch (e) { /* non-fatal */ }
      setLaunchItem('li-server', 'done');
    } else if (probe.status === 401) {
      setLaunchItem('li-server', 'error', 'token rejected (401)');
      appendLaunchLog('✗ Server at ' + cfg.serverUrl + ' rejected the token (401). Check that COLLAB_TOKEN matches what the server was started with.', true);
      resetLaunchBtn();
      return;
    }
    // Any other status (5xx, unexpected) → fall through and try to spawn locally.
  } catch (e) {
    // Connect refused / DNS failure / timeout → no server reachable, we'll spawn one.
    appendLaunchLog('• No server reachable at ' + cfg.serverUrl + ' — will start one locally', false);
  }

  if (!serverAlreadyRunning) {
    appendLaunchLog('Starting collab-server on ' + cfg.serverUrl, false);
    try {
      await invoke('start_server', {
        serverUrl:  cfg.serverUrl,
        adminToken: sessionAdminToken,
        projectDir: cfg.projectDir,
      });
      // Give the server a moment to start
      await sleep(1200);
      setLaunchItem('li-server', 'done');
      appendLaunchLog('✓ Server started', false);
    } catch (e) {
      setLaunchItem('li-server', 'error', e);
      appendLaunchLog('✗ ' + e, true);
      resetLaunchBtn();
      return;
    }
  }

  // Mint a team token against the running server if the wizard didn't
  // collect one. For localhost single-user this is the default path — the
  // server starts with no COLLAB_TOKEN configured, so /admin/teams accepts
  // the request without auth and hands back a tm_… token we can use
  // everywhere downstream.
  if (!cfg.token) {
    appendLaunchLog(`Minting team token for "${cfg.teamName}"…`, false);
    try {
      cfg.token = await mintTeamTokenDuringLaunch(sessionAdminToken);
      setEnv('COLLAB_TOKEN', cfg.token);
      appendLaunchLog('✓ Team token minted', false);
    } catch (e) {
      setLaunchItem('li-server', 'error', 'team mint failed');
      appendLaunchLog('✗ ' + (e && e.message ? e.message : e), true);
      resetLaunchBtn();
      return;
    }
  }

  // Step 3: collab init. The CLI sniffs the yaml and picks the team-init
  // code path because our file starts with `team:`, writing AGENT.md +
  // .collab/team-managed into each worker's codebase_path.
  setLaunchItem('li-init', 'running');
  appendLaunchLog('Running: collab init team.yml', false);
  try {
    const code = await invoke('run_command', {
      program: 'collab',
      args:    ['init', 'team.yml'],
      cwd:     cfg.projectDir,
      envs,
    });
    if (code !== 0) throw new Error(`collab init exited with code ${code}`);
    setLaunchItem('li-init', 'done');
    appendLaunchLog('✓ Worker environments created', false);
  } catch (e) {
    setLaunchItem('li-init', 'error', e);
    appendLaunchLog('✗ ' + e, true);
    launchHadError = true;
    // Non-fatal — continue
  }

  // Step 4: collab start all
  setLaunchItem('li-workers', 'running');
  appendLaunchLog('Running: collab start all', false);
  try {
    const code = await invoke('run_command', {
      program: 'collab',
      args:    ['start', 'all'],
      cwd:     cfg.projectDir,
      envs,
    });
    if (code !== 0 && code !== null) {
      appendLaunchLog(`collab start all exited with code ${code} (workers may still start)`, false);
    }
    setLaunchItem('li-workers', 'done');
    appendLaunchLog('✓ Workers started', false);
  } catch (e) {
    setLaunchItem('li-workers', 'error', e);
    appendLaunchLog('✗ ' + e, true);
    launchHadError = true;
  }

  // Save config
  cfg.setupComplete = true;
  try {
    // Persist the full config including the token. The token lives in a
    // user-only file under ~/.config/hold-my-beer-gui/ — the same trust level
    // as your SSH keys and shell history — and the alternative (re-entering a
    // 64-char hex token every launch) is hostile for a localhost dev tool.
    await invoke('save_config', { config: cfg });
  } catch (e) { /* non-fatal */ }

  document.getElementById('open-dash-btn').hidden = false;
  document.getElementById('back-from-4').disabled = false;
  if (launchHadError) {
    appendLaunchLog(
      '\n⚠ Launch finished with errors — review the log above, then click ' +
      '"Open Dashboard" or "Back" to fix settings.',
      true
    );
    resetLaunchBtn();
  } else {
    appendLaunchLog('\n🚀 Ready! Opening dashboard…', false);
    setTimeout(toDashboard, 800);
  }
}

function setLaunchItem(id, state, detail = '') {
  const el = document.getElementById(id);
  if (!el) return;
  el.className = 'launch-item ' + state;
  const icon = el.querySelector('.li-icon');
  icon.textContent = state === 'done' ? '✓' : state === 'error' ? '✗' : state === 'running' ? '⏳' : '⏳';
  const d = el.querySelector('.li-detail');
  if (d && detail) d.textContent = String(detail).slice(0, 80);
}

function resetLaunchBtn() {
  document.getElementById('launch-btn').hidden = false;
  document.getElementById('back-from-4').disabled = false;
}

function appendLaunchLog(line, isErr) {
  const log = document.getElementById('launch-log');
  if (!log) return;
  const span = document.createElement('span');
  span.className = isErr ? 'log-err' : '';
  span.textContent = line + '\n';
  log.appendChild(span);
  log.scrollTop = log.scrollHeight;
}

function clearLaunchLog() {
  const log = document.getElementById('launch-log');
  if (log) log.innerHTML = '';
}

// Emit team.yml (current backend schema). Each worker owns its own
// `codebase_path`; when the user didn't override, we fall back to the
// project folder picked in Step 2 so the yaml is always valid.
function buildTeamYaml() {
  const yamlStr = s => String(s).replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\r?\n|\r/g, ' ');
  const lines = [];
  lines.push(`team: "${yamlStr(cfg.teamName)}"`);
  lines.push(`server: "${yamlStr(cfg.serverUrl)}"`);
  lines.push(`cli_template: "${yamlStr(cfg.cliTemplate)}"`);
  if (cfg.model) lines.push(`model: "${yamlStr(cfg.model)}"`);
  lines.push(`workers:`);
  for (const w of workers) {
    const cbPath = (w.codebase_path && w.codebase_path.trim()) || cfg.projectDir;
    lines.push(`  - name: ${w.name}`);
    lines.push(`    role: "${yamlStr(w.role)}"`);
    lines.push(`    codebase_path: "${yamlStr(cbPath)}"`);
    if (w.model) lines.push(`    model: "${yamlStr(w.model)}"`);
    if (w.cli_template) lines.push(`    cli_template: "${yamlStr(w.cli_template)}"`);
  }
  return lines.join('\n') + '\n';
}

// ── Wizard → Dashboard ────────────────────────────────────────────────────────
function toDashboard() {
  if (dashboardActive) return;
  dashboardActive = true;
  // Prevent a second call (e.g. the 800ms auto-timer racing with a button click).
  document.getElementById('open-dash-btn').hidden = true;

  const wiz  = document.getElementById('wizard');
  const dash = document.getElementById('dashboard');
  wiz.classList.add('exiting');
  setTimeout(() => {
    wiz.hidden = true;
    dash.hidden = false;
    requestAnimationFrame(() => dash.classList.add('visible'));
    showDashboard();
  }, 350);
}

function goToWizard() {
  wizardFromDashboard = dashboardActive;
  dashboardActive = false;
  const wiz  = document.getElementById('wizard');
  const dash = document.getElementById('dashboard');
  dash.classList.remove('visible');
  setTimeout(() => {
    dash.hidden = true;
    wiz.hidden = false;
    wiz.classList.remove('exiting');
    teardownDashboard();
    prefillWizard();
    goStep(1);
    updateWizardStep1Nav();
  }, 350);
}

// ── Dashboard ─────────────────────────────────────────────────────────────────
function showDashboard() {
  // Pre-fill compose from identity
  const from = document.getElementById('compose-from');
  if (from && cfg.identity) from.value = cfg.identity;

  // Update server URL badge
  const badge = document.getElementById('server-url-badge');
  if (badge) badge.textContent = cfg.serverUrl.replace('http://', '').replace('https://', '');

  // Register presence heartbeat (as GUI observer)
  registerPresence();

  // Start polling roster + todos
  fetchRoster();
  fetchTodos();
  rosterTimer = setInterval(fetchRoster, 30_000);
  todosTimer  = setInterval(fetchTodos, 15_000);
  serverPollTimer = setInterval(pollServerStatus, 5_000);

  // Connect SSE
  connectSSE();

  // Keyboard shortcut for compose (added once; teardownDashboard removes it)
  const textEl = document.getElementById('compose-text');
  if (textEl) {
    textEl.removeEventListener('keydown', onComposeKeydown);
    textEl.addEventListener('keydown', onComposeKeydown);
  }
}

function teardownDashboard() {
  clearInterval(rosterTimer);
  clearInterval(todosTimer);
  clearInterval(serverPollTimer);
  clearInterval(presenceTimer);
  presenceTimer = null;
  if (usageTimer) { clearInterval(usageTimer); usageTimer = null; }
  usageOpen = false;
  const usagePanel = document.getElementById('usage-panel');
  if (usagePanel) usagePanel.classList.remove('open');
  if (sseConn) { sseConn.abort(); sseConn = null; }
  const textEl = document.getElementById('compose-text');
  if (textEl) textEl.removeEventListener('keydown', onComposeKeydown);
}

// ── SSE connection ────────────────────────────────────────────────────────────
// Uses fetch + ReadableStream instead of EventSource so the bearer token is
// sent in an Authorization header rather than as a URL query parameter.
async function connectSSE() {
  if (sseConn) { sseConn.abort(); sseConn = null; }

  const controller = new AbortController();
  sseConn = controller;

  try {
    const res = await fetch(`${cfg.serverUrl}/events`, {
      headers: { Authorization: `Bearer ${cfg.token}` },
      signal: controller.signal,
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    sseRetries = 0;
    setConnStatus(true);

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = '';

    // eslint-disable-next-line no-constant-condition
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      // SSE messages are separated by blank lines; split on \n\n
      const parts = buf.split('\n\n');
      buf = parts.pop(); // keep any incomplete trailing chunk
      for (const block of parts) {
        for (const line of block.split('\n')) {
          if (line.startsWith('data: ')) {
            try {
              const msg = JSON.parse(line.slice(6));
              onNewMessage(msg);
            } catch (_) {}
          }
        }
      }
    }
  } catch (e) {
    if (e.name === 'AbortError') return; // intentional teardown
    setConnStatus(false);
    sseConn = null;
    scheduleSSEReconnect();
    return;
  }
  // Stream ended cleanly — server closed the connection.
  setConnStatus(false);
  sseConn = null;
  scheduleSSEReconnect();
}

function scheduleSSEReconnect() {
  // Startup case dominates this code path: the server sidecar was spawned
  // ~instantly ago and is still binding :8000, so the first few connects
  // fail with "connection refused". Aggressive initial retries (≤500ms)
  // close the gap — by the time we're 3–4 retries in, the server is ready.
  //
  // Steady-state-failure case (server crashed, network broken) doesn't
  // benefit from sub-second retries; slow growth past retry ~6 walks the
  // delay up to a reasonable 10s ceiling without the 30s plateau the
  // previous schedule sat on.
  const delay = Math.min(
    200 * Math.pow(1.5, sseRetries) + Math.random() * 100,
    10_000,
  );
  sseRetries++;
  setTimeout(connectSSE, delay);
}

function setConnStatus(up) {
  const pill  = document.getElementById('conn-pill');
  const label = document.getElementById('conn-label');
  if (!pill) return;
  pill.className = 'conn-pill ' + (up ? 'live' : 'dead');
  // "connecting…" before we've ever been up; "reconnecting…" after we've
  // had a live connection and it dropped. Cleaner UX than always showing
  // "reconnecting…" during the initial sidecar boot.
  if (label) {
    if (up) {
      label.textContent = 'live';
      window.__sseEverLive = true;
    } else {
      label.textContent = window.__sseEverLive ? 'reconnecting…' : 'connecting…';
    }
  }
}

// ── Server status polling ─────────────────────────────────────────────────────
async function pollServerStatus() {
  const running = await invoke('server_running').catch(() => false);
  const dot = document.getElementById('server-dot');
  const startBtn = document.getElementById('btn-start-server');
  const stopBtn  = document.getElementById('btn-stop-server');
  if (!dot) return;
  dot.className = 'server-dot ' + (running ? 'up' : '');
  if (startBtn) startBtn.hidden = running;
  if (stopBtn)  stopBtn.hidden  = !running;
}

async function doStartServer() {
  try {
    await invoke('start_server', {
      serverUrl:  cfg.serverUrl,
      token:      cfg.token,
      projectDir: cfg.projectDir,
    });
    toast('Server started.', false);
    pollServerStatus();
    connectSSE();
  } catch (e) {
    toast('Server error: ' + e, true);
  }
}

async function doStopServer() {
  try {
    await invoke('stop_server');
    toast('Server stopped.', false);
    pollServerStatus();
  } catch (e) {
    toast('Stop error: ' + e, true);
  }
}

// ── Roster ────────────────────────────────────────────────────────────────────
async function fetchRoster() {
  try {
    const res = await fetch(`${cfg.serverUrl}/roster`, {
      headers: { Authorization: `Bearer ${cfg.token}` },
    });
    if (!res.ok) return;
    const data = await res.json();
    renderRoster(data);
  } catch (_) {}
}

function renderRoster(workers) {
  // Keep a global list of known workers for autocomplete / todo assignment.
  if (workers && workers.length) {
    knownWorkers = workers.map(w => ({ instance_id: w.instance_id, role: w.role || '' }));
    updateTodoWorkerSelect();
  }

  const list = document.getElementById('roster-list');
  if (!list) return;
  list.innerHTML = '';
  const countEl = document.getElementById('roster-header-count');
  if (countEl) countEl.textContent = workers && workers.length ? String(workers.length) : '';
  if (!workers || workers.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'feed-empty feed-empty-sm';
    empty.textContent = 'No workers online';
    list.appendChild(empty);
    return;
  }
  const now = Date.now();
  workers.forEach(w => {
    const lastSeen = new Date(w.last_seen).getTime();
    const isOnline = (now - lastSeen) < 90_000; // 90s threshold
    const colorIdx = getColor(w.instance_id);

    const item = document.createElement('div');
    item.className = 'roster-item';
    item.title = w.role || '';
    item.dataset.worker = w.instance_id;
    // Click opens the "is it working?" drill-in. Kick button below gets
    // stopPropagation so the click doesn't bubble to this handler.
    item.addEventListener('click', () => {
      openWorkerGlance(w.instance_id, w.role || '');
    });

    const dot = document.createElement('div');
    dot.className = 'roster-dot' + (isOnline ? ' online' : '');
    item.appendChild(dot);

    const body = document.createElement('div');
    body.className = 'roster-body';
    const name = document.createElement('div');
    name.className = 'roster-name';
    name.style.color = COLORS[colorIdx];
    name.textContent = w.instance_id;
    body.appendChild(name);
    if (w.role) {
      const role = document.createElement('div');
      role.className = 'roster-role';
      role.textContent = w.role;
      body.appendChild(role);
    }
    item.appendChild(body);

    const count = document.createElement('span');
    count.className = 'roster-count';
    count.textContent = w.message_count || '';
    item.appendChild(count);

    // Manual kick button — only for workers that aren't us. Sends a tiny
    // message that the worker harness treats like any other external
    // delivery, triggering one CLI call against its backlog. Exists because
    // we deliberately don't idle-kick (idle must stay free), so a stalled
    // worker with pending todos needs a human poke.
    if (w.instance_id !== (cfg.identity || 'human')) {
      const kick = document.createElement('button');
      kick.type = 'button';
      kick.className = 'roster-kick';
      kick.textContent = '⚡';
      kick.title = `Kick @${w.instance_id} — fires one CLI call against their pending todos`;
      kick.addEventListener('click', (e) => {
        e.stopPropagation();
        kickWorker(w.instance_id, kick);
      });
      item.appendChild(kick);
    }

    list.appendChild(item);
  });
}

async function kickWorker(instanceId, btn) {
  if (!cfg.serverUrl || !cfg.token) { toast('Not connected', true); return; }
  const sender = (cfg.identity || 'human').replace(/^@/, '');
  const origText = btn.textContent;
  btn.disabled = true;
  btn.textContent = '…';
  try {
    const res = await fetch(`${cfg.serverUrl}/messages`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${cfg.token}` },
      body: JSON.stringify({
        sender,
        recipient: instanceId,
        content: 'Checking in — process any pending todos or reply with blockers.',
        refs: [],
      }),
    });
    if (res.ok) {
      toast(`Kicked @${instanceId}`);
      // Visible win: hold the button at ✓ green and tint the row briefly so
      // a hover-mouse-away user still sees that the kick fired. Revert
      // after 1.2s so repeat kicks remain available.
      btn.textContent = '✓';
      btn.classList.add('ok');
      const row = btn.closest('.roster-item');
      if (row) {
        row.classList.add('kicked');
        setTimeout(() => row.classList.remove('kicked'), 1200);
      }
      setTimeout(() => {
        btn.textContent = origText;
        btn.classList.remove('ok');
        btn.disabled = false;
      }, 1200);
    } else {
      toast(`Kick failed: ${res.status}`, true);
      btn.textContent = origText;
      btn.disabled = false;
    }
  } catch (e) {
    toast(`Kick error: ${e}`, true);
    btn.textContent = origText;
    btn.disabled = false;
  }
}

// ── Todos ─────────────────────────────────────────────────────────────────────
async function fetchTodos() {
  if (!cfg.token) return;
  // Gather all worker ids known to us (live roster + wizard config).
  const seen = new Set();
  knownWorkers.forEach(w => seen.add(w.instance_id));
  workers.forEach(w => { if (w.name) seen.add(w.name); });
  // Fall back to own identity if nothing else is known yet.
  if (seen.size === 0 && cfg.identity) seen.add(cfg.identity.replace(/^@/, ''));
  if (seen.size === 0) return;

  try {
    const allTodos = [];
    await Promise.all([...seen].map(async id => {
      if (!/^[A-Za-z0-9_-]{1,64}$/.test(id)) return;
      try {
        const res = await fetch(`${cfg.serverUrl}/todos/${id}`, {
          headers: { Authorization: `Bearer ${cfg.token}` },
        });
        if (!res.ok) return;
        const todos = await res.json();
        allTodos.push(...todos);
      } catch (_) {}
    }));
    allTodos.sort((a, b) => new Date(a.created_at) - new Date(b.created_at));
    renderTodos(allTodos);
  } catch (_) {}
}

function renderTodos(todos) {
  const list  = document.getElementById('todos-list');
  const count = document.getElementById('todos-count');
  if (!list) return;
  if (count) count.textContent = todos.length;
  list.innerHTML = '';
  if (todos.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'todos-empty';
    empty.textContent = 'No pending tasks';
    list.appendChild(empty);
    return;
  }
  todos.forEach(t => {
    const div = document.createElement('div');
    div.className = 'todo-item';
    div.dataset.hash = t.hash || '';

    const byLine = document.createElement('div');
    byLine.className = 'todo-by';
    const assignee = document.createElement('span');
    assignee.className = 'todo-assignee';
    assignee.textContent = '@' + (t.instance || '');
    byLine.appendChild(assignee);
    byLine.appendChild(document.createTextNode(' · from ' + (t.assigned_by || '') + ' · ' + timeAgo(t.created_at)));

    // ✓ button — marks the todo complete via PATCH /todos/:hash/done.
    // Server treats "complete" as the only mutation (no separate delete),
    // and a completed todo drops out of list_todos on the next fetch.
    if (t.hash) {
      const done = document.createElement('button');
      done.className = 'todo-done';
      done.title = 'Mark done — removes it from this worker\'s queue';
      done.setAttribute('aria-label', 'Mark todo complete');
      done.textContent = '✓';
      done.addEventListener('click', (e) => {
        e.stopPropagation();
        markTodoDone(t.hash, div, done);
      });
      byLine.appendChild(done);
    }
    div.appendChild(byLine);

    const descLine = document.createElement('div');
    descLine.className = 'todo-desc';
    descLine.textContent = t.description || '';
    div.appendChild(descLine);

    list.appendChild(div);
  });
}

// Mark a todo complete. Optimistic: fade the row immediately so the click
// feels instant; on server failure we restore it and toast the error.
async function markTodoDone(hash, rowEl, btnEl) {
  if (!cfg.serverUrl || !cfg.token) { toast('Not connected', true); return; }
  if (!hash || hash.length < 4) return;
  btnEl.disabled = true;
  rowEl.classList.add('completing');
  try {
    const res = await fetch(`${cfg.serverUrl}/todos/${encodeURIComponent(hash)}/done`, {
      method: 'PATCH',
      headers: { Authorization: `Bearer ${cfg.token}` },
    });
    if (res.ok) {
      // Animate out, then refresh the list so other clients (and ourselves)
      // pick up the new state from the server rather than guessing.
      setTimeout(() => fetchTodos(), 220);
    } else if (res.status === 409) {
      // Already completed by someone else — refresh to drop the row.
      fetchTodos();
    } else {
      rowEl.classList.remove('completing');
      btnEl.disabled = false;
      toast('Could not mark done: HTTP ' + res.status, true);
    }
  } catch (e) {
    rowEl.classList.remove('completing');
    btnEl.disabled = false;
    toast('Could not mark done: ' + e, true);
  }
}

function updateTodoWorkerSelect() {
  const sel = document.getElementById('todo-assign-to');
  if (!sel) return;
  const prev = sel.value;
  sel.innerHTML = '<option value="">Assign to…</option>';

  // Build a merged list: live roster first, then wizard-configured workers as
  // fallback so the dropdown works before the server is running.
  const seen = new Set();
  const entries = [];
  knownWorkers.forEach(w => {
    if (!seen.has(w.instance_id)) { seen.add(w.instance_id); entries.push({ id: w.instance_id, role: w.role }); }
  });
  workers.forEach(w => {
    if (w.name && !seen.has(w.name)) { seen.add(w.name); entries.push({ id: w.name, role: w.role || '' }); }
  });

  entries.forEach(({ id, role }) => {
    const opt = document.createElement('option');
    opt.value = id;
    opt.textContent = id + (role ? ` — ${role}` : '');
    sel.appendChild(opt);
  });
  if (prev) sel.value = prev;
}

function toggleTodoForm() {
  const form = document.getElementById('todo-compose');
  if (!form) return;
  form.hidden = !form.hidden;
  if (!form.hidden) {
    updateTodoWorkerSelect();
    const desc = document.getElementById('todo-desc');
    if (desc) desc.focus();
  }
}

async function doAddTodo() {
  const sel  = document.getElementById('todo-assign-to');
  const desc = document.getElementById('todo-desc');
  if (!sel || !desc) return;
  const instance = sel.value.trim();
  const description = desc.value.trim();
  if (!instance) { toast('Choose a worker to assign the task to', true); return; }
  if (!description) { toast('Enter a task description', true); return; }
  if (description.length > 500) { toast('Task description must be 500 characters or fewer', true); return; }
  // Validate that the selected worker is known (prevent arbitrary instance injection).
  const validWorkerIds = new Set([
    ...knownWorkers.map(w => w.instance_id),
    ...workers.map(w => w.name).filter(Boolean),
  ]);
  if (validWorkerIds.size > 0 && !validWorkerIds.has(instance)) {
    toast('Unknown worker: ' + instance, true); return;
  }
  const assignedBy = cfg.identity || 'gui';
  try {
    const res = await fetch(`${cfg.serverUrl}/todos`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization:  `Bearer ${cfg.token}`,
      },
      body: JSON.stringify({ assigned_by: assignedBy, instance, description }),
    });
    if (res.ok) {
      desc.value = '';
      sel.value = '';
      const form = document.getElementById('todo-compose');
      if (form) form.hidden = true;
      toast(`Task assigned to @${instance}`);
      fetchTodos();
    } else {
      toast('Failed to add task: ' + res.status, true);
    }
  } catch (e) {
    toast('Error adding task: ' + e, true);
  }
}

// ── Messages ──────────────────────────────────────────────────────────────────
function onNewMessage(msg) {
  // Avoid duplicates
  if (allMessages.some(m => m.id === msg.id)) return;
  allMessages.push(msg);

  // Track mentions
  if (cfg.identity && msg.recipient === cfg.identity && activeTab !== 'mentions') {
    unreadMentions++;
    updateMentionBadge();
  }

  const el = buildMessageEl(msg, true);
  const feed = document.getElementById('feed');
  if (!feed) return;

  // Remove empty state
  const empty = document.getElementById('feed-empty');
  if (empty) empty.remove();

  // Only show if matches current tab (must match rerenderFeed's filter exactly)
  if (activeTab === 'mentions' && msg.recipient !== cfg.identity) {
    return;
  }

  feed.appendChild(el);
  feed.scrollTop = feed.scrollHeight;

  // Update count
  const countEl = document.getElementById('msg-count');
  if (countEl) countEl.textContent = `${allMessages.length} msg${allMessages.length !== 1 ? 's' : ''}`;
}

function buildMessageEl(msg, isNew) {
  const colorIdx = getColor(msg.sender);
  const div = document.createElement('div');
  div.className = 'msg' + (isNew ? ' is-new' : '');

  const badge = document.createElement('span');
  badge.className = `msg-badge badge-${colorIdx}`;
  badge.textContent = msg.sender || '';
  div.appendChild(badge);

  const meta = document.createElement('div');
  meta.className = 'msg-meta';

  const senderSpan = document.createElement('span');
  senderSpan.className = `msg-sender c${colorIdx}`;
  senderSpan.textContent = msg.sender || '';
  meta.appendChild(senderSpan);

  const toSpan = document.createElement('span');
  toSpan.className = 'msg-to';
  toSpan.textContent = (msg.recipient && msg.recipient !== 'all') ? `→ ${msg.recipient}` : '→ all';
  meta.appendChild(toSpan);

  const timeSpan = document.createElement('span');
  timeSpan.className = 'msg-time';
  timeSpan.textContent = fmtTime(msg.timestamp);
  meta.appendChild(timeSpan);

  div.appendChild(meta);

  const body = document.createElement('div');
  body.className = 'msg-body';
  body.textContent = msg.content || '';
  div.appendChild(body);

  return div;
}

function setTab(tab) {
  activeTab = tab;
  document.querySelectorAll('.feed-tab').forEach(el => {
    el.classList.toggle('active', el.id === 'tab-' + tab);
  });
  if (tab === 'mentions') {
    unreadMentions = 0;
    updateMentionBadge();
  }
  rerenderFeed();
}

function rerenderFeed() {
  const feed = document.getElementById('feed');
  if (!feed) return;
  feed.innerHTML = '';
  const msgs = activeTab === 'mentions'
    ? allMessages.filter(m => m.recipient === cfg.identity)
    : allMessages;
  if (msgs.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'feed-empty';
    empty.id = 'feed-empty';
    empty.textContent = activeTab === 'mentions' ? 'No mentions yet.' : 'No messages yet.';
    feed.appendChild(empty);
    return;
  }
  msgs.forEach(m => feed.appendChild(buildMessageEl(m, false)));
  feed.scrollTop = feed.scrollHeight;
}

function updateMentionBadge() {
  const badge = document.getElementById('mention-badge');
  if (!badge) return;
  badge.hidden = unreadMentions === 0;
  badge.textContent = unreadMentions;
}

// ── Compose ───────────────────────────────────────────────────────────────────
// ── @ Mention autocomplete ────────────────────────────────────────────────────
let mentionMatches = [];
let mentionIdx     = 0;
// Cursor position within the word being completed (start of @token in textarea)
let mentionStart   = -1;

function getWorkerNames() {
  // Live roster first; fall back to wizard-configured workers so autocomplete
  // works even before the server is running.
  const seen = new Set(['all']);
  knownWorkers.forEach(w => seen.add(w.instance_id));
  workers.forEach(w => { if (w.name) seen.add(w.name); });
  return [...seen];
}

function renderMentionList() {
  const list = document.getElementById('mention-list');
  if (!list) return;
  if (!mentionMatches.length) { list.hidden = true; list.innerHTML = ''; return; }
  list.hidden = false;
  list.innerHTML = '';
  mentionMatches.forEach((name, i) => {
    const li = document.createElement('li');
    li.className = 'slash-item' + (i === mentionIdx ? ' selected' : '');
    li.setAttribute('role', 'option');
    if (i === mentionIdx) li.setAttribute('aria-selected', 'true');
    li.dataset.name = name;
    const nameSpan = document.createElement('span');
    nameSpan.className = 'slash-cmd';
    nameSpan.textContent = '@' + name;
    li.appendChild(nameSpan);
    const w = knownWorkers.find(w => w.instance_id === name);
    if (w && w.role) {
      const roleSpan = document.createElement('span');
      roleSpan.className = 'slash-desc';
      roleSpan.textContent = w.role;
      li.appendChild(roleSpan);
    }
    list.appendChild(li);
  });
}

function closeMentionList() {
  mentionMatches = [];
  mentionIdx = 0;
  mentionStart = -1;
  const list = document.getElementById('mention-list');
  if (list) { list.hidden = true; list.innerHTML = ''; }
}

function applyMention(name) {
  const input = document.getElementById('compose-text');
  if (!input || mentionStart < 0) return;
  const val = input.value;
  // Find where the current @word ends
  let end = mentionStart + 1;
  while (end < val.length && /\S/.test(val[end])) end++;
  const before = val.slice(0, mentionStart);
  const after  = val.slice(end);
  input.value = before + '@' + name + ' ' + after;
  const pos = mentionStart + name.length + 2; // after '@name '
  input.setSelectionRange(pos, pos);
  closeMentionList();
  input.focus();
}

// ── Slash commands ────────────────────────────────────────────────────────────
const SLASH_CMDS = [
  { cmd: 'r',   desc: 'Reply to last sender',  hint: '/r [text]'            },
  { cmd: 'w',   desc: 'DM a worker',           hint: '/w <worker> [text]'   },
  { cmd: 'all', desc: 'Message everyone',      hint: '/all [text]'          },
];
let slashMatches = [];
let slashIdx = 0;

// Find the most recent sender who addressed me (or anyone else as fallback).
// Drives the `/r` shortcut. Uses `allMessages` which the SSE stream fills.
function lastSenderForMe() {
  const me = (cfg.identity || '').trim();
  for (let i = allMessages.length - 1; i >= 0; i--) {
    const m = allMessages[i];
    if (m.sender !== me && (m.recipient === me || m.recipient === 'all')) return m.sender;
  }
  for (let i = allMessages.length - 1; i >= 0; i--) {
    if (allMessages[i].sender !== me) return allMessages[i].sender;
  }
  return null;
}

function renderSlashList() {
  const list = document.getElementById('slash-list');
  if (!list) return;
  if (!slashMatches.length) { list.hidden = true; list.innerHTML = ''; return; }
  list.hidden = false;
  list.innerHTML = '';
  slashMatches.forEach((c, i) => {
    const li = document.createElement('li');
    li.className = 'slash-item';
    li.setAttribute('role', 'option');
    if (i === slashIdx) li.setAttribute('aria-selected', 'true');
    li.dataset.cmd = c.cmd;

    const cmdSpan = document.createElement('span');
    cmdSpan.className = 'slash-cmd';
    cmdSpan.textContent = '/' + c.cmd;
    li.appendChild(cmdSpan);

    const descSpan = document.createElement('span');
    descSpan.className = 'slash-desc';
    descSpan.textContent = c.desc;
    li.appendChild(descSpan);

    const hintSpan = document.createElement('span');
    hintSpan.className = 'slash-hint';
    hintSpan.textContent = c.hint;
    li.appendChild(hintSpan);

    list.appendChild(li);
  });
}

function closeSlashList() {
  slashMatches = [];
  slashIdx = 0;
  const list = document.getElementById('slash-list');
  if (list) { list.hidden = true; list.innerHTML = ''; }
}

// Auto-expand when a slash command is picked from the palette or space-completed.
function applySlash(cmd) {
  const input = document.getElementById('compose-text');
  if (!input) return;
  closeSlashList();
  if (cmd === 'r') {
    const sender = lastSenderForMe();
    if (sender) {
      input.value = `@${sender} `;
    } else {
      input.value = '';
      toast('No recent messages to reply to', true);
    }
  } else if (cmd === 'all') {
    input.value = '@all ';
  } else if (cmd === 'w') {
    input.value = '@';
  }
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
}

// Used at send time: `/r hello` → `@sender hello`. Returns null to abort the send.
function expandSlashOnSend(text) {
  if (!text.startsWith('/')) return text;
  const spIdx = text.indexOf(' ');
  const cmd   = spIdx === -1 ? text.slice(1) : text.slice(1, spIdx);
  const rest  = spIdx === -1 ? '' : text.slice(spIdx + 1).trim();
  if (cmd === 'r') {
    const sender = lastSenderForMe();
    if (!sender) { toast('No recent messages to reply to', true); return null; }
    return rest ? `@${sender} ${rest}` : `@${sender} `;
  }
  if (cmd === 'all') return rest ? `@all ${rest}` : '@all ';
  if (cmd === 'w') {
    const parts = rest.split(/\s+/);
    const worker = parts[0];
    if (!worker) { toast('Usage: /w <worker> [message]', true); return null; }
    const body = parts.slice(1).join(' ');
    return body ? `@${worker} ${body}` : `@${worker} `;
  }
  return text; // unknown — let it go through as-is
}

function onComposeInput() {
  const input = document.getElementById('compose-text');
  if (!input) return;
  const val = input.value;
  const cursor = input.selectionStart;

  // Show palette while typing `/cmd` with no space yet.
  if (val.startsWith('/') && !val.includes(' ')) {
    closeMentionList();
    const q = val.slice(1).toLowerCase();
    slashMatches = SLASH_CMDS.filter(c => c.cmd.startsWith(q));
    slashIdx = 0;
    renderSlashList();
    return;
  }
  closeSlashList();

  // Auto-expand once the user types a space after a recognized slash command.
  if (val === '/r ')   { applySlash('r');   return; }
  if (val === '/all ') { applySlash('all'); return; }
  if (val === '/w ')   { applySlash('w');   return; }

  // @ mention autocomplete: find the @word that the cursor is inside.
  let atPos = -1;
  for (let i = cursor - 1; i >= 0; i--) {
    if (val[i] === '@') { atPos = i; break; }
    if (/\s/.test(val[i])) break;
  }
  if (atPos >= 0) {
    const query = val.slice(atPos + 1, cursor).toLowerCase();
    const names = getWorkerNames();
    mentionMatches = names.filter(n => n.toLowerCase().startsWith(query));
    if (mentionMatches.length) {
      mentionStart = atPos;
      mentionIdx = 0;
      renderMentionList();
      return;
    }
  }
  closeMentionList();
}

function onComposeKeydown(e) {
  // Mention palette navigation
  if (mentionMatches.length) {
    if (e.key === 'Enter' || e.key === 'Tab') {
      e.preventDefault();
      applyMention(mentionMatches[mentionIdx]);
      return;
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      mentionIdx = (mentionIdx + 1) % mentionMatches.length;
      renderMentionList();
      return;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      mentionIdx = (mentionIdx - 1 + mentionMatches.length) % mentionMatches.length;
      renderMentionList();
      return;
    }
    if (e.key === 'Escape') { closeMentionList(); return; }
  }

  // Slash palette navigation
  if (slashMatches.length) {
    if (e.key === 'Enter' || e.key === 'Tab') {
      e.preventDefault();
      applySlash(slashMatches[slashIdx].cmd);
      return;
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      slashIdx = (slashIdx + 1) % slashMatches.length;
      renderSlashList();
      return;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      slashIdx = (slashIdx - 1 + slashMatches.length) % slashMatches.length;
      renderSlashList();
      return;
    }
    if (e.key === 'Escape') { closeSlashList(); return; }
  }

  // Enter sends, Shift+Enter adds a newline
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    doSendMessage();
  }
}

async function doSendMessage() {
  const fromEl = document.getElementById('compose-from');
  const toEl   = document.getElementById('compose-to');
  const textEl = document.getElementById('compose-text');
  if (!fromEl || !toEl || !textEl) return;

  const sender = fromEl.value.trim() || cfg.identity || 'gui';
  let rawText  = textEl.value.trim();
  if (!rawText) return;

  // Expand slash commands at send time (e.g. `/r hi` → `@alice hi`).
  if (rawText.startsWith('/')) {
    const expanded = expandSlashOnSend(rawText);
    if (expanded === null) return;
    rawText = expanded.trim();
  }

  // If the message starts with @name, treat that as the recipient override.
  let content = rawText;
  let recipient = (toEl.value.trim() || 'all').replace(/^@/, '');
  const mentionMatch = rawText.match(/^@(\S+)\s*(.*)$/s);
  if (mentionMatch) {
    recipient = mentionMatch[1];
    content = mentionMatch[2];
    // Validate recipient against known workers (allow 'all' and own identity).
    const knownIds = new Set(['all', cfg.identity || '', ...knownWorkers.map(w => w.instance_id), ...workers.map(w => w.name).filter(Boolean)]);
    if (!knownIds.has(recipient)) {
      toast(`Unknown recipient: @${recipient}`, true); return;
    }
  }
  if (!content) return;
  if (content.length > 4000) { toast('Message must be 4000 characters or fewer', true); return; }

  try {
    const res = await fetch(`${cfg.serverUrl}/messages`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization:  `Bearer ${cfg.token}`,
      },
      body: JSON.stringify({ sender, recipient, content, refs: [] }),
    });
    if (res.ok) {
      textEl.value = '';
      textEl.focus();
    } else {
      toast('Send failed: ' + res.status, true);
    }
  } catch (e) {
    toast('Send error: ' + e, true);
  }
}

// ── Worker lifecycle ──────────────────────────────────────────────────────────
async function doStartWorkers() {
  const envs = [
    ['COLLAB_TOKEN', cfg.token],
    ['COLLAB_SERVER', cfg.serverUrl],
    ['COLLAB_INSTANCE', cfg.identity || 'gui'],
  ];
  try {
    await invoke('run_command', {
      program: 'collab',
      args:    ['start', 'all'],
      cwd:     cfg.projectDir || undefined,
      envs,
    });
    toast('Workers starting…', false);
    setTimeout(fetchRoster, 2000);
  } catch (e) {
    toast('Error starting workers: ' + e, true);
  }
}

async function doStopWorkers() {
  const envs = [
    ['COLLAB_TOKEN', cfg.token],
    ['COLLAB_SERVER', cfg.serverUrl],
    ['COLLAB_INSTANCE', cfg.identity || 'gui'],
  ];
  try {
    await invoke('run_command', {
      program: 'collab',
      args:    ['stop', 'all'],
      cwd:     cfg.projectDir || undefined,
      envs,
    });
    toast('Workers stopping…', false);
  } catch (e) {
    toast('Error: ' + e, true);
  }
}

async function doBroadcastStop() {
  if (!confirm('Stop all workers?')) return;
  // Actually kill the worker processes — the old "broadcast a polite message"
  // approach relied on the LLM honoring a STOP message, which it never did.
  await doStopWorkers();
}

// ── Presence heartbeat ────────────────────────────────────────────────────────
async function registerPresence() {
  if (!cfg.identity) return;
  const identity = cfg.identity.replace(/^@/, '');
  async function heartbeat() {
    try {
      await fetch(`${cfg.serverUrl}/presence/${identity}`, {
        method: 'PUT',
        headers: {
          'Content-Type': 'application/json',
          Authorization:  `Bearer ${cfg.token}`,
        },
        body: JSON.stringify({ role: 'GUI observer' }),
      });
    } catch (_) {}
  }
  heartbeat();
  presenceTimer = setInterval(heartbeat, 30_000);
}

// ── Panel toggles ─────────────────────────────────────────────────────────────
// The three visibility panels (log, todos, usage) route through `prefs` so
// that the topbar buttons and the accessibility panel stay in sync, and the
// state survives a restart. applyPrefs() does the DOM work.
function toggleServerLog() {
  prefs.panels.log = !prefs.panels.log;
  savePrefs(); applyPrefs();
}

function toggleTodos() {
  prefs.panels.todos = !prefs.panels.todos;
  savePrefs(); applyPrefs();
}

// ── Usage panel ───────────────────────────────────────────────────────────────
function toggleUsage() {
  prefs.panels.usage = !prefs.panels.usage;
  savePrefs(); applyPrefs();
  // Poll only while the panel is visible.
  if (prefs.panels.usage) {
    fetchUsage();
    if (!usageTimer) usageTimer = setInterval(fetchUsage, 10_000);
  } else {
    if (usageTimer) { clearInterval(usageTimer); usageTimer = null; }
  }
}

async function fetchUsage() {
  if (!cfg.serverUrl) { renderUsage(null); return; }
  try {
    const url = cfg.serverUrl.replace(/\/+$/, '') + '/usage';
    const headers = cfg.token ? { Authorization: `Bearer ${cfg.token}` } : {};
    const resp = await fetch(url, { headers });
    if (!resp.ok) { renderUsage(null); return; }
    const data = await resp.json();
    renderUsage(data);
  } catch (e) {
    renderUsage(null);
  }
}

function renderUsage(data) {
  const inner = document.getElementById('usage-inner');
  const totalEl = document.getElementById('usage-total');
  if (!inner) return;
  inner.innerHTML = '';

  const workers = (data && Array.isArray(data.workers)) ? data.workers : [];
  if (!workers.length) {
    const empty = document.createElement('div');
    empty.className = 'usage-empty';
    empty.textContent = data
      ? 'No usage recorded yet. Workers report to the server after each turn.'
      : 'Could not reach the server.';
    inner.appendChild(empty);
    if (totalEl) totalEl.textContent = '—';
    return;
  }

  const totalIn = data.total_input_tokens || 0;
  const totalOut = data.total_output_tokens || 0;
  const totalDur = data.total_duration_secs || 0;
  const totalCost = data.total_cost_usd || 0;
  const totalCalls = data.total_calls || 0;

  if (totalEl) {
    const costStr = totalCost > 0 ? ` · $${totalCost.toFixed(4)}` : '';
    totalEl.textContent =
      `${totalCalls} call${totalCalls !== 1 ? 's' : ''} · ` +
      `${fmtTokens(totalIn + totalOut)} toks · ` +
      `${fmtDuration(totalDur)}${costStr}`;
  }

  // One row per worker — heaviest first (server already sorts by token volume).
  workers.forEach(w => {
    const row = document.createElement('div');
    row.className = 'usage-row';

    const nameEl = document.createElement('span');
    nameEl.className = 'usage-worker';
    nameEl.textContent = w.worker;
    nameEl.style.color = COLORS[getColor(w.worker)];
    row.appendChild(nameEl);

    const toks = document.createElement('span');
    toks.className = 'usage-toks';
    toks.textContent = `${fmtTokens(w.input_tokens)} in · ${fmtTokens(w.output_tokens)} out`;
    row.appendChild(toks);

    const calls = document.createElement('span');
    calls.className = 'usage-time';
    calls.textContent = `${w.calls} call${w.calls !== 1 ? 's' : ''} (${w.full_calls}F/${w.light_calls}L)`;
    row.appendChild(calls);

    const dur = document.createElement('span');
    dur.className = 'usage-dur';
    dur.textContent = fmtDuration(w.duration_secs);
    row.appendChild(dur);

    const cost = document.createElement('span');
    cost.className = 'usage-cost' + (w.cost_usd > 0 ? '' : ' zero');
    cost.textContent = w.cost_usd > 0 ? `$${w.cost_usd.toFixed(4)}` : '—';
    row.appendChild(cost);

    inner.appendChild(row);
  });

  const sep = document.createElement('div');
  sep.className = 'usage-sep';
  inner.appendChild(sep);

  const totalRow = document.createElement('div');
  totalRow.className = 'usage-row usage-totals-row';
  totalRow.innerHTML =
    `<span class="usage-worker" style="color:var(--dim-text)">TOTAL</span>` +
    `<span class="usage-toks">${fmtTokens(totalIn)} in · ${fmtTokens(totalOut)} out</span>` +
    `<span class="usage-time" style="color:var(--dim-text)">${totalCalls} call${totalCalls !== 1 ? 's' : ''}</span>` +
    `<span class="usage-dur">${fmtDuration(totalDur)}</span>` +
    `<span class="usage-cost${totalCost > 0 ? '' : ' zero'}">${totalCost > 0 ? '$' + totalCost.toFixed(4) : '—'}</span>`;
  inner.appendChild(totalRow);
}

function fmtTokens(n) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
  if (n >= 1_000)     return (n / 1_000).toFixed(1) + 'k';
  return String(n);
}

function fmtDuration(secs) {
  if (secs < 60) return secs + 's';
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}m${s.toString().padStart(2, '0')}s`;
}

function appendServerLog(line, isErr) {
  const inner = document.getElementById('server-log-inner');
  if (!inner) return;
  const span = document.createElement('span');
  span.className = isErr ? 'log-err' : '';
  span.textContent = line + '\n';
  inner.appendChild(span);
  // Keep max 500 lines
  while (inner.children.length > 500) inner.removeChild(inner.firstChild);
  inner.scrollTop = inner.scrollHeight;
}

// ── Utilities ─────────────────────────────────────────────────────────────────
const COLORS = ['#38bdf8','#fb7185','#34d399','#a78bfa','#e879f9','#2dd4bf'];

function getColor(name) {
  if (senderColors[name] === undefined) {
    senderColors[name] = colorCounter % COLORS.length;
    colorCounter++;
  }
  return senderColors[name];
}

function esc(s) {
  return String(s || '')
    .replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')
    .replace(/"/g,'&quot;').replace(/'/g,'&#39;');
}

function fmtTime(iso) {
  try {
    const d = new Date(iso);
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  } catch (_) { return ''; }
}

function timeAgo(iso) {
  const diff = (Date.now() - new Date(iso).getTime()) / 1000;
  if (diff < 60)   return 'just now';
  if (diff < 3600) return Math.floor(diff / 60) + 'm ago';
  if (diff < 86400) return Math.floor(diff / 3600) + 'h ago';
  return Math.floor(diff / 86400) + 'd ago';
}

function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

function toast(msg, isErr) {
  const el = document.createElement('div');
  el.className = 'toast ' + (isErr ? 'err' : 'ok');
  el.textContent = msg;
  document.getElementById('toasts').appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

// Prefs load + apply is called at the bottom of the file, after the
// `let prefs = ...` declaration has run — calling it up here would hit the
// TDZ and crash the whole script.

// ── Test hooks ───────────────────────────────────────────────────────────────
// Playwright tests need to read/write wizard state and call internal
// functions, but `let cfg` / `let workers` don't attach to window in a
// classic <script>. Expose a minimal bridge — token is intentionally excluded
// from the getter to avoid leaking credentials via window inspection.
window.__wizard = {
  get cfg()        { const { token: _t, ...safe } = cfg; return safe; },
  set cfg(v)       { const { token: _t, ...rest } = v; Object.assign(cfg, rest); },
  get workers()    { return workers; },
  set workers(v)   { workers = v; },
  parseTeamOrWorkersYaml,
  buildTeamYaml,
  loadExistingProject,
  syncWorkersFromDom,
  addWorker,
  removeWorker,
  step1Next,
  step2Next,
  step3Next,
  doLaunch,
};

// ── Event wiring (replaces former inline on*= handlers) ─────────────────────
// CSP strict mode forbids inline event attributes, so every button below was
// given an id/data-attr in index.html and is bound here instead.
(function wireEvents() {
  const on = (id, ev, fn) => {
    const el = document.getElementById(id);
    if (el) el.addEventListener(ev, fn);
  };

  // Block webview reload shortcuts — F5, Ctrl+R, Ctrl+Shift+R, Cmd+R.
  // A reload wipes in-memory GUI state but Rust-side worker handles survive,
  // which leaves the user with zombie workers and no UI to manage them.
  window.addEventListener('keydown', (e) => {
    const isReloadKey =
      e.key === 'F5' ||
      ((e.ctrlKey || e.metaKey) && (e.key === 'r' || e.key === 'R'));
    if (isReloadKey) {
      e.preventDefault();
      e.stopPropagation();
    }
  }, true);

  // Wizard — step navigation + actions
  on('btn-generate-team-token','click', doGenerateTeamToken);
  on('btn-paste-team-token','click', doPasteTeamToken);
  on('btn-step1-next',      'click', step1Next);
  on('btn-browse',          'click', doBrowse);
  on('btn-step2-next',  'click', step2Next);
  on('btn-step3-next',  'click', step3Next);
  on('btn-add-worker',  'click', addWorker);
  on('launch-btn',      'click', doLaunch);
  on('open-dash-btn',   'click', toDashboard);

  // Generic "goStep" back buttons — any element with data-goto="N"
  document.querySelectorAll('[data-goto]').forEach(el => {
    el.addEventListener('click', () => goStep(parseInt(el.dataset.goto, 10)));
  });

  // Step 3 tool dropdown
  const toolEl = document.getElementById('s3-tool');
  if (toolEl) toolEl.addEventListener('change', onToolChange);

  // Dashboard — topbar
  on('btn-start-server',  'click', doStartServer);
  on('btn-stop-server',   'click', doStopServer);
  on('btn-start-workers', 'click', doStartWorkers);
  on('btn-stop-workers',  'click', doStopWorkers);
  on('btn-toggle-log',    'click', toggleServerLog);
  on('btn-toggle-usage',  'click', toggleUsage);
  on('btn-toggle-todos',  'click', toggleTodos);
  on('btn-to-wizard',     'click', goToWizard);

  // Dashboard — roster + feed + compose
  on('btn-broadcast-stop', 'click', doBroadcastStop);
  on('btn-send',           'click', doSendMessage);

  // Feed tabs — data-tab attribute
  document.querySelectorAll('[data-tab]').forEach(el => {
    el.addEventListener('click', () => setTab(el.dataset.tab));
  });

  // Compose — slash command palette
  const textEl = document.getElementById('compose-text');
  if (textEl) textEl.addEventListener('input', onComposeInput);
  const slashList = document.getElementById('slash-list');
  if (slashList) {
    slashList.addEventListener('mousedown', e => {
      const item = e.target.closest('.slash-item');
      if (item) { e.preventDefault(); applySlash(item.dataset.cmd); }
    });
  }
  if (textEl) {
    textEl.addEventListener('blur', () => {
      // Delay so mousedown on a palette item can fire first.
      setTimeout(() => { closeSlashList(); closeMentionList(); }, 150);
    });
  }

  // Mention list click
  const mentionListEl = document.getElementById('mention-list');
  if (mentionListEl) {
    mentionListEl.addEventListener('mousedown', e => {
      const item = e.target.closest('.slash-item');
      if (item) { e.preventDefault(); applyMention(item.dataset.name); }
    });
  }

  // Todo form toggle + submit
  on('btn-todo-toggle-form', 'click', toggleTodoForm);
  on('btn-add-todo',         'click', doAddTodo);
  const todoDesc = document.getElementById('todo-desc');
  if (todoDesc) {
    todoDesc.addEventListener('keydown', e => {
      if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); doAddTodo(); }
    });
  }

  // Design refresh (2026-04-20): theme toggle, a11y panel, mic, resize,
  // worker drill-in. Only mic + resize wire at load; a11y panel and worker
  // glance wire inside app-panels.js when that bundle is pulled in by
  // ensurePanels().
  on('btn-toggle-theme', 'click', toggleTheme);
  on('btn-toggle-a11y',  'click', toggleA11yPanel);
  wireMic();
  wireResize();

  // Auto-theme: follow system changes if user chose 'auto'.
  if (window.matchMedia) {
    window.matchMedia('(prefers-color-scheme: light)').addEventListener('change', () => {
      if (prefs.theme === 'auto') applyPrefs();
    });
  }
})();

// ═══════════════════════════════════════════════════════════════════════════
// PREFERENCES (theme / font / size / density / accent / panels / motion)
// Single versioned store in localStorage under `hmb.prefs`. Versioned so the
// shape can evolve without wiping the user's choices — bump PREFS_VERSION
// and add a migration branch in loadPrefs() instead of ignoring v-mismatch.
// ═══════════════════════════════════════════════════════════════════════════
const PREFS_KEY = 'hmb.prefs';
const PREFS_VERSION = 1;
const DEFAULT_PREFS = {
  v: PREFS_VERSION,
  theme: 'saloon',      // 'saloon' | 'daylight' | 'auto'
  font: 'default',      // 'default' | 'readable' | 'dyslexic' | 'system'
  size: 14,             // px, clamped 12–20
  density: 'comfortable',
  accent: null,         // null → theme default; else a hex like '#f0a830'
  panels: { roster: true, todos: false, usage: false, log: false },
  widths:  { roster: 200, todos: 280 },
  reduceMotion: false,
};
let prefs = { ...DEFAULT_PREFS, panels: { ...DEFAULT_PREFS.panels }, widths: { ...DEFAULT_PREFS.widths } };

const FONT_STACKS = {
  default:  { sans: "'Manrope', ui-sans-serif, system-ui, sans-serif",
              mono: "'JetBrains Mono', 'Fira Code', ui-monospace, monospace" },
  readable: { sans: "'Atkinson Hyperlegible', 'Inter', ui-sans-serif, sans-serif",
              mono: "'JetBrains Mono', ui-monospace, monospace" },
  dyslexic: { sans: "'OpenDyslexic', 'Atkinson Hyperlegible', sans-serif",
              mono: "'OpenDyslexicMono', 'JetBrains Mono', ui-monospace, monospace" },
  system:   { sans: "ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
              mono: "ui-monospace, Menlo, Consolas, monospace" },
};

function loadPrefs() {
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (!raw) return;
    const obj = JSON.parse(raw);
    if (!obj || typeof obj !== 'object' || obj.v !== PREFS_VERSION) return;
    prefs = {
      ...DEFAULT_PREFS,
      ...obj,
      panels: { ...DEFAULT_PREFS.panels, ...(obj.panels || {}) },
      widths: { ...DEFAULT_PREFS.widths, ...(obj.widths || {}) },
    };
  } catch (_) { /* fall back to defaults */ }
}

function savePrefs() {
  try { localStorage.setItem(PREFS_KEY, JSON.stringify(prefs)); } catch (_) {}
}

function resolveTheme() {
  if (prefs.theme === 'auto') {
    return (window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches)
      ? 'daylight' : 'saloon';
  }
  return prefs.theme === 'daylight' ? 'daylight' : 'saloon';
}

function applyPrefs() {
  const html = document.documentElement;
  html.setAttribute('data-theme', resolveTheme());
  html.setAttribute('data-reduce-motion', prefs.reduceMotion ? '1' : '0');
  html.setAttribute('data-density', prefs.density);

  const stack = FONT_STACKS[prefs.font] || FONT_STACKS.default;
  html.style.setProperty('--font-sans', stack.sans);
  html.style.setProperty('--font-mono', stack.mono);
  html.style.setProperty('--app-size', (prefs.size || 14) + 'px');

  // Accent override. Null → remove so the theme default wins.
  if (prefs.accent) html.style.setProperty('--accent', prefs.accent);
  else              html.style.removeProperty('--accent');

  html.style.setProperty('--roster-w', (prefs.widths.roster || 200) + 'px');
  html.style.setProperty('--todos-w',  (prefs.widths.todos  || 280) + 'px');

  // Panel visibility — keep legacy globals in sync so existing code keeps
  // working (and don't double-fire timers for the usage panel).
  const roster = document.getElementById('roster');
  const rosterHandle = document.getElementById('resize-roster');
  if (roster) roster.classList.toggle('collapsed', !prefs.panels.roster);
  if (rosterHandle) rosterHandle.hidden = !prefs.panels.roster;

  const todos = document.getElementById('todos-panel');
  const todosHandle = document.getElementById('resize-todos');
  if (todos) todos.classList.toggle('collapsed', !prefs.panels.todos);
  if (todosHandle) todosHandle.hidden = !prefs.panels.todos;
  todosVisible = !!prefs.panels.todos;

  const usageEl = document.getElementById('usage-panel');
  if (usageEl) usageEl.classList.toggle('open', !!prefs.panels.usage);
  usageOpen = !!prefs.panels.usage;

  const logEl = document.getElementById('server-log-panel');
  if (logEl) logEl.classList.toggle('open', !!prefs.panels.log);
  serverLogOpen = !!prefs.panels.log;

  reflectPrefsIntoA11yPanel();
}

// Everything that reads/writes the a11y panel UI lives in app-panels.js and
// is loaded on demand by ensurePanels(). These thin stubs forward to the
// panel module once it's been hydrated; before then they're safe no-ops
// so applyPrefs() can call reflectPrefsIntoA11yPanel() unconditionally.
function reflectPrefsIntoA11yPanel() {
  if (window.__panels) window.__panels.reflectPrefs();
  // The topbar theme-button icon state is critical-path; keep it here so
  // the active class tracks resolveTheme() even before panels are loaded.
  const themeBtn = document.getElementById('btn-toggle-theme');
  if (themeBtn) themeBtn.classList.toggle('active', resolveTheme() === 'daylight');
}

function toggleA11yPanel() {
  ensurePanels().then(() => window.__panels.toggleA11y());
}

function toggleTheme() {
  // Quick flip in the topbar. 'auto' resolves to a concrete dark/light first
  // so the next click stays predictable. Doesn't need the a11y panel loaded.
  const current = resolveTheme();
  prefs.theme = current === 'daylight' ? 'saloon' : 'daylight';
  savePrefs(); applyPrefs();
}

// ───────── Deferred panel loader ─────────
// Inject app-panels.min.css + app-panels.min.js on first demand. Cached so
// subsequent calls resolve immediately. Keeping both off the initial HTML
// is what gets Lighthouse back to 100 — they're ~300 CSS + ~500 JS lines
// that index.html would otherwise pay for at paint time.
let __panelsPromise = null;
function ensurePanels() {
  if (window.__panels) return Promise.resolve();
  if (__panelsPromise) return __panelsPromise;
  __panelsPromise = new Promise((resolve, reject) => {
    const link = document.createElement('link');
    link.rel  = 'stylesheet';
    link.href = 'app-panels.min.css';
    document.head.appendChild(link);

    const script = document.createElement('script');
    script.src = 'app-panels.min.js';
    script.onload  = () => resolve();
    script.onerror = () => reject(new Error('Failed to load app-panels.min.js'));
    document.head.appendChild(script);
  });
  return __panelsPromise;
}

// ═══════════════════════════════════════════════════════════════════════════
// MIC — browser SpeechRecognition. Per the designer's note, when the API
// isn't available the button stays visible but disabled with a tooltip
// pointing at OS-native dictation so the feature doesn't silently vanish.
// ═══════════════════════════════════════════════════════════════════════════
function wireMic() {
  const btn = document.getElementById('btn-mic');
  if (!btn) return;
  const SR = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!SR) {
    btn.disabled = true;
    btn.title =
      'Voice input unavailable in this webview.\n' +
      'Use your OS dictation: macOS Fn×2, Windows Win+H, iOS/Android keyboard mic.';
    return;
  }

  const rec = new SR();
  rec.continuous = true;
  rec.interimResults = true;
  rec.lang = navigator.language || 'en-US';

  let state = 'idle';
  let baseText = '';
  let finalBuf = '';

  function setIdle() {
    state = 'idle';
    btn.classList.remove('listening');
    btn.title = 'Voice input (browser speech-to-text)';
  }
  function setListening() {
    state = 'listening';
    btn.classList.add('listening');
    btn.title = 'Listening — click to stop';
    const ta = document.getElementById('compose-text');
    baseText = ta ? ta.value : '';
    finalBuf = '';
  }

  rec.onstart  = setListening;
  rec.onend    = setIdle;
  rec.onresult = (e) => {
    let interim = '';
    for (let i = e.resultIndex; i < e.results.length; i++) {
      const r = e.results[i];
      if (r.isFinal) finalBuf += r[0].transcript;
      else interim += r[0].transcript;
    }
    const ta = document.getElementById('compose-text');
    if (!ta) return;
    const sep = baseText && !/\s$/.test(baseText) ? ' ' : '';
    ta.value = baseText + sep + finalBuf + interim;
    // Fire the input handler so slash/mention autocomplete keeps working
    // while the user dictates.
    ta.dispatchEvent(new Event('input'));
  };
  rec.onerror = (e) => {
    if (e.error === 'not-allowed' || e.error === 'service-not-allowed') {
      toast('Mic permission denied — enable it in your browser settings.', true);
    } else if (e.error === 'no-speech') {
      // Normal timeout; don't bother the user.
    } else {
      toast('Mic error: ' + e.error, true);
    }
    setIdle();
  };

  btn.addEventListener('click', () => {
    if (state === 'listening') { try { rec.stop(); } catch (_) {} }
    else                       { try { rec.start(); } catch (_) {} }
  });
}

// ═══════════════════════════════════════════════════════════════════════════
// RESIZE HANDLES — drag between roster/feed/todos. Width persists in prefs;
// double-click resets to the default. Collapsed state is tracked
// independently via prefs.panels.{roster,todos}.
// ═══════════════════════════════════════════════════════════════════════════
function wireResize() {
  document.querySelectorAll('.resize-handle').forEach(handle => {
    const target = handle.dataset.target; // 'roster' | 'todos'
    if (!target) return;

    handle.addEventListener('mousedown', (e) => {
      e.preventDefault();
      const layout = document.querySelector('.layout');
      if (!layout) return;
      const startX = e.clientX;
      const startW = prefs.widths[target] || (target === 'roster' ? 200 : 280);
      // Roster is on the left (drag right → wider); todos is on the right
      // (drag left → wider). The sign flips accordingly.
      const sign   = target === 'roster' ? +1 : -1;
      const minW   = target === 'roster' ? 140 : 220;
      const maxW   = Math.max(260, Math.floor(layout.getBoundingClientRect().width - 320));

      handle.classList.add('dragging');
      document.body.style.userSelect = 'none';

      function onMove(ev) {
        const dx = (ev.clientX - startX) * sign;
        const w = Math.max(minW, Math.min(maxW, startW + dx));
        prefs.widths[target] = Math.round(w);
        document.documentElement.style.setProperty(
          target === 'roster' ? '--roster-w' : '--todos-w',
          prefs.widths[target] + 'px'
        );
      }
      function onUp() {
        handle.classList.remove('dragging');
        document.body.style.userSelect = '';
        document.removeEventListener('mousemove', onMove);
        document.removeEventListener('mouseup', onUp);
        savePrefs();
      }
      document.addEventListener('mousemove', onMove);
      document.addEventListener('mouseup', onUp);
    });

    handle.addEventListener('dblclick', () => {
      prefs.widths[target] = target === 'roster' ? 200 : 280;
      savePrefs(); applyPrefs();
    });
  });
}


// ═══════════════════════════════════════════════════════════════════════════
// WORKER GLANCE — "Is it working?" drill-in (deferred)
// Full implementation (render, mock data, wiring, sparkline, git tags) lives
// in app-panels.js. Core app only keeps these forwarding stubs so a roster
// click can open the panel without paying for the ~500 lines of glance code
// at initial paint. ensurePanels() injects the deferred bundle on first use.
// ═══════════════════════════════════════════════════════════════════════════
function openWorkerGlance(workerName, workerRole) {
  ensurePanels().then(() => window.__panels.openGlance(workerName, workerRole));
}

function closeWorkerGlance() {
  if (window.__panels) window.__panels.closeGlance();
}


// Final init: now that `prefs` + helpers are all declared, hydrate from
// localStorage and paint the first theme/font/panel state. Runs after the
// wireEvents IIFE above — event listeners are bound, prefs are applied.
loadPrefs();
applyPrefs();
