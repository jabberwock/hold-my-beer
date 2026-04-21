'use strict';

// ═══════════════════════════════════════════════════════════════════════════
// app-panels.js — deferred bundle.
// Lazy-loaded by app.js's ensurePanels() on first click of the a11y or
// theme button (or first click on a roster row). Carries the a11y panel
// + worker-glance DOM, styles, and wire code so the critical path doesn't
// pay for Lighthouse's unused-js / unused-css penalties at initial paint.
//
// This script references top-level bindings from app.js directly (prefs,
// savePrefs, applyPrefs, resolveTheme, toggleServerLog, fetchUsage,
// usageTimer, toast). Those live in the shared classic-script scope so no
// imports are needed.
// ═══════════════════════════════════════════════════════════════════════════

(function initPanels() {
  if (window.__panels) return; // idempotent — ensurePanels won't reload

  // ── Build the two floating panels into <body>. Keeps index.html tiny. ──
  function buildA11yPanel() {
    const p = document.createElement('div');
    p.id = 'a11y-panel';
    p.hidden = true;
    p.setAttribute('role', 'dialog');
    p.setAttribute('aria-label', 'Display and accessibility');
    p.innerHTML = `
      <div class="a11y-head">
        <span class="a11y-mug">🍺</span>
        <span class="a11y-head-title">Pour it how you like</span>
        <button class="btn-icon-sm" id="a11y-close" aria-label="Close">✕</button>
      </div>
      <div class="a11y-body">
        <div class="a11y-group">
          <div class="a11y-group-label">Theme</div>
          <div class="a11y-seg" data-pref="theme" role="tablist">
            <button class="a11y-seg-opt" data-val="saloon" role="tab">Dark</button>
            <button class="a11y-seg-opt" data-val="daylight" role="tab">Light</button>
            <button class="a11y-seg-opt" data-val="auto" role="tab">Auto</button>
          </div>
        </div>
        <div class="a11y-group">
          <div class="a11y-group-label">Font</div>
          <div class="a11y-font-list" data-pref="font">
            <button class="a11y-font-opt" data-val="default">
              <span class="a11y-font-radio"></span>
              <span class="a11y-font-meta">
                <span class="a11y-font-name font-default">Manrope</span>
                <span class="a11y-font-sub">Default · The quick brown fox</span>
              </span>
            </button>
            <button class="a11y-font-opt" data-val="readable">
              <span class="a11y-font-radio"></span>
              <span class="a11y-font-meta">
                <span class="a11y-font-name font-readable">Atkinson Hyperlegible</span>
                <span class="a11y-font-sub font-readable">High-clarity · The quick brown fox</span>
              </span>
            </button>
            <button class="a11y-font-opt" data-val="dyslexic">
              <span class="a11y-font-radio"></span>
              <span class="a11y-font-meta">
                <span class="a11y-font-name font-dyslexic">OpenDyslexic</span>
                <span class="a11y-font-sub font-dyslexic">Dyslexia-friendly · The quick brown fox</span>
              </span>
              <span class="a11y-font-badge">A11Y</span>
            </button>
            <button class="a11y-font-opt" data-val="system">
              <span class="a11y-font-radio"></span>
              <span class="a11y-font-meta">
                <span class="a11y-font-name font-system">System UI</span>
                <span class="a11y-font-sub font-system">Your OS default · The quick brown fox</span>
              </span>
            </button>
          </div>
        </div>
        <div class="a11y-group">
          <div class="a11y-group-label">Text size</div>
          <div class="a11y-size-row">
            <span class="a11y-size-a a11y-size-sm">A</span>
            <input id="a11y-size" type="range" min="12" max="20" step="1" value="14">
            <span class="a11y-size-a a11y-size-lg">A</span>
          </div>
          <div class="a11y-size-marks">
            <span>12</span><span>14</span><span>16</span><span>18</span><span>20</span>
          </div>
        </div>
        <div class="a11y-group">
          <div class="a11y-group-label">Density</div>
          <div class="a11y-seg" data-pref="density">
            <button class="a11y-seg-opt" data-val="compact">Compact</button>
            <button class="a11y-seg-opt" data-val="comfortable">Comfortable</button>
          </div>
        </div>
        <div class="a11y-group">
          <div class="a11y-group-label">Accent</div>
          <div class="a11y-accent-swatches" data-pref="accent">
            <button class="a11y-accent acc-amber"  data-val="#f0a830" title="Amber (default)"></button>
            <button class="a11y-accent acc-saddle" data-val="#d4604a" title="Saddle red"></button>
            <button class="a11y-accent acc-hop"    data-val="#8bbf5f" title="Hop green"></button>
            <button class="a11y-accent acc-teal"   data-val="#63b3b8" title="Copper teal"></button>
            <button class="a11y-accent acc-plum"   data-val="#b07cc7" title="Plum"></button>
            <button class="a11y-accent acc-dark"   data-val="#b8860b" title="Dark amber"></button>
          </div>
        </div>
        <div class="a11y-group">
          <div class="a11y-group-label">Panels</div>
          <div class="a11y-toggle-list">
            <button class="a11y-toggle" data-toggle="roster"><span class="a11y-toggle-switch"></span>Roster (left)</button>
            <button class="a11y-toggle" data-toggle="todos"><span class="a11y-toggle-switch"></span>Todos (right)</button>
            <button class="a11y-toggle" data-toggle="usage"><span class="a11y-toggle-switch"></span>Token usage</button>
            <button class="a11y-toggle" data-toggle="log"><span class="a11y-toggle-switch"></span>Server log</button>
            <button class="a11y-toggle" data-toggle="reduceMotion"><span class="a11y-toggle-switch"></span>Reduce motion</button>
          </div>
        </div>
      </div>
    `;
    document.body.appendChild(p);
  }

  function buildWorkerGlance() {
    const p = document.createElement('div');
    p.id = 'worker-glance';
    p.hidden = true;
    p.setAttribute('role', 'dialog');
    p.setAttribute('aria-label', 'Worker detail');
    p.innerHTML = `
      <div class="glance-head">
        <div class="glance-avatar" id="glance-avatar">?</div>
        <div class="glance-title-wrap">
          <div class="glance-title">
            <span class="glance-name" id="glance-name">—</span>
            <span class="glance-pill" id="glance-state">● IDLE</span>
            <span class="glance-pid" id="glance-pid"></span>
          </div>
          <div class="glance-meta">
            <span class="glance-role" id="glance-role">—</span>
            <span class="glance-dot">·</span>
            <span class="glance-cwd" id="glance-cwd">—</span>
          </div>
        </div>
        <div class="glance-actions">
          <button class="btn btn-ghost btn-sm" id="glance-dm">DM</button>
          <button class="btn btn-ghost btn-sm" id="glance-logs">Logs</button>
          <button class="btn btn-danger btn-sm" id="glance-stop">Stop</button>
          <button class="btn btn-ghost btn-sm" id="glance-close" aria-label="Close">✕</button>
        </div>
      </div>
      <div class="glance-stats">
        <div>
          <div class="glance-stat-label">Last output</div>
          <div class="glance-stat-row">
            <span class="glance-stat-value warn" id="glance-last-age">—</span>
            <span class="glance-stat-sub" id="glance-last-sub">ago</span>
          </div>
        </div>
        <div>
          <div class="glance-stat-label">Uncommitted</div>
          <div class="glance-stat-row">
            <span class="glance-stat-value info" id="glance-uncommitted">0</span>
            <span class="glance-stat-sub">files changed</span>
          </div>
        </div>
        <div>
          <div class="glance-stat-label">Cost today</div>
          <div class="glance-stat-row">
            <span class="glance-stat-value accent" id="glance-cost">—</span>
            <span class="glance-stat-sub" id="glance-calls">0 calls</span>
          </div>
        </div>
        <div>
          <div class="glance-sparkline-head">
            <span>Activity · last 30 min</span>
            <span>tokens/min</span>
          </div>
          <div class="glance-sparkline" id="glance-sparkline"></div>
        </div>
      </div>
      <div class="glance-cols">
        <div class="glance-col">
          <div class="glance-col-head">
            <span class="glance-col-title">Last output</span>
            <span class="glance-col-hint" id="glance-output-hint">from claude</span>
          </div>
          <div class="glance-output" id="glance-output"></div>
          <div class="glance-tail">
            <span class="glance-tail-dot"></span>
            <span class="glance-tail-label">tail -f · mocked</span>
            <div class="feed-spacer"></div>
            <button class="btn btn-ghost btn-xs" id="glance-open-log">full log →</button>
          </div>
        </div>
        <div class="glance-col">
          <div class="glance-col-head">
            <span class="glance-col-title">Uncommitted</span>
            <span class="glance-col-hint" id="glance-git-hint">—</span>
          </div>
          <div class="glance-git-list" id="glance-git-list"></div>
          <div class="glance-git-foot-note">
            Git actions will land here once the server exposes
            <span class="mono">/git/status</span> and
            <span class="mono">/git/commit</span> per worker.
          </div>
        </div>
        <div class="glance-col">
          <div class="glance-col-head">
            <span class="glance-col-title">Todos</span>
            <span class="glance-col-hint" id="glance-todos-hint">0 done · 0 open</span>
          </div>
          <div id="glance-todos"></div>
        </div>
      </div>
    `;
    document.body.appendChild(p);
  }

  // ── A11y panel ──────────────────────────────────────────────
  function reflectPrefsIntoA11yPanel() {
    document.querySelectorAll('[data-pref="theme"] .a11y-seg-opt').forEach(btn => {
      btn.classList.toggle('active', btn.dataset.val === prefs.theme);
    });
    document.querySelectorAll('[data-pref="density"] .a11y-seg-opt').forEach(btn => {
      btn.classList.toggle('active', btn.dataset.val === prefs.density);
    });
    document.querySelectorAll('[data-pref="font"] .a11y-font-opt').forEach(btn => {
      btn.classList.toggle('active', btn.dataset.val === prefs.font);
    });
    const activeAccent = prefs.accent || '#f0a830';
    document.querySelectorAll('[data-pref="accent"] .a11y-accent').forEach(btn => {
      btn.classList.toggle('active', btn.dataset.val === activeAccent);
    });
    const sizeInput = document.getElementById('a11y-size');
    if (sizeInput && Number(sizeInput.value) !== prefs.size) sizeInput.value = prefs.size;

    document.querySelectorAll('.a11y-toggle').forEach(btn => {
      const key = btn.dataset.toggle;
      let on = false;
      if (key === 'reduceMotion') on = !!prefs.reduceMotion;
      else if (key in prefs.panels) on = !!prefs.panels[key];
      btn.classList.toggle('on', on);
    });

    const themeBtn = document.getElementById('btn-toggle-theme');
    if (themeBtn) themeBtn.classList.toggle('active', resolveTheme() === 'daylight');
  }

  function toggleA11yPanel() {
    const p = document.getElementById('a11y-panel');
    if (!p) return;
    p.hidden = !p.hidden;
    if (!p.hidden) {
      reflectPrefsIntoA11yPanel();
      // Only one floating surface at a time.
      const g = document.getElementById('worker-glance');
      if (g && !g.hidden) closeWorkerGlance();
    }
  }

  function wireA11yPanel() {
    document.querySelectorAll('.a11y-seg').forEach(seg => {
      seg.addEventListener('click', e => {
        const btn = e.target.closest('.a11y-seg-opt');
        if (!btn) return;
        const key = seg.dataset.pref;
        if (!key) return;
        prefs[key] = btn.dataset.val;
        savePrefs(); applyPrefs();
      });
    });

    const fontList = document.querySelector('[data-pref="font"]');
    if (fontList) fontList.addEventListener('click', e => {
      const btn = e.target.closest('.a11y-font-opt');
      if (!btn) return;
      prefs.font = btn.dataset.val;
      savePrefs(); applyPrefs();
    });

    const accentGrid = document.querySelector('[data-pref="accent"]');
    if (accentGrid) accentGrid.addEventListener('click', e => {
      const btn = e.target.closest('.a11y-accent');
      if (!btn) return;
      // Clicking the already-active amber swatch clears the override.
      if (prefs.accent === btn.dataset.val && btn.dataset.val === '#f0a830') prefs.accent = null;
      else prefs.accent = btn.dataset.val;
      savePrefs(); applyPrefs();
    });

    const sz = document.getElementById('a11y-size');
    if (sz) {
      sz.addEventListener('input', () => {
        prefs.size = Math.max(12, Math.min(20, Number(sz.value) || 14));
        applyPrefs();
      });
      sz.addEventListener('change', savePrefs);
    }

    document.querySelectorAll('.a11y-toggle').forEach(btn => {
      btn.addEventListener('click', () => {
        const key = btn.dataset.toggle;
        if (!key) return;
        if (key === 'reduceMotion') prefs.reduceMotion = !prefs.reduceMotion;
        else if (key in prefs.panels) prefs.panels[key] = !prefs.panels[key];
        savePrefs();
        // Usage panel has a polling timer — re-use the existing toggle path
        // so it starts/stops correctly. Other panels: applyPrefs is enough.
        if (key === 'usage') {
          if (prefs.panels.usage) {
            fetchUsage();
            if (!usageTimer) usageTimer = setInterval(fetchUsage, 10_000);
          } else if (usageTimer) {
            clearInterval(usageTimer); usageTimer = null;
          }
        }
        applyPrefs();
      });
    });

    const closeBtn = document.getElementById('a11y-close');
    if (closeBtn) closeBtn.addEventListener('click', () => {
      const p = document.getElementById('a11y-panel'); if (p) p.hidden = true;
    });

    document.addEventListener('mousedown', (e) => {
      const p = document.getElementById('a11y-panel');
      if (!p || p.hidden) return;
      if (p.contains(e.target)) return;
      if (e.target.closest('#btn-toggle-a11y')) return;
      p.hidden = true;
    });
  }

  // ── Worker glance ───────────────────────────────────────────
  let glanceOpenFor = null;

  function _hashSeed(s) {
    let h = 2166136261 >>> 0;
    for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619) >>> 0; }
    return () => {
      h = Math.imul(h ^ (h >>> 15), 2246822507) >>> 0;
      h = Math.imul(h ^ (h >>> 13), 3266489909) >>> 0;
      h ^= (h >>> 16);
      return (h >>> 0) / 4294967296;
    };
  }

  function _mockGlance(workerName, liveRole) {
    const rnd = _hashSeed(workerName || 'worker');
    const ages = ['3s', '11s', '42s', '2m', '6m'];
    const pidPool = [24123, 31780, 42981, 69027, 77125, 88432];
    const fileStems = [
      'src/app.js', 'src/index.ts', 'public/styles.css',
      'tests/smoke.spec.mjs', 'README.md', 'docs/howto.md', 'build.sh',
    ];
    const tagPool = ['M', 'M', 'A', 'M', '?'];
    const gitCount = Math.floor(rnd() * 4) + 2;
    const gitChanges = [];
    for (let i = 0; i < gitCount; i++) {
      const tag = tagPool[Math.floor(rnd() * tagPool.length)];
      gitChanges.push({
        tag,
        path: fileStems[Math.floor(rnd() * fileStems.length)],
        plus:  tag === 'D' ? 0 : Math.floor(rnd() * 40) + 1,
        minus: tag === 'A' ? 0 : Math.floor(rnd() * 15),
      });
    }
    const activity = Array.from({ length: 30 }, () => Math.floor(rnd() * 11));
    for (let i = 27; i < 30; i++) activity[i] = Math.floor(rnd() * 8) + 4;

    return {
      name: workerName,
      role: liveRole || 'Worker',
      pid: pidPool[Math.floor(rnd() * pidPool.length)],
      state: 'working',
      cwd: `~/code/${workerName}`,
      branch: `work/${workerName}`,
      lastOutputAge: ages[Math.floor(rnd() * ages.length)],
      cost: '$' + (0.5 + rnd() * 9.5).toFixed(4),
      calls: Math.floor(rnd() * 40) + 6,
      output: [
        { t: '16:21:03', text: `read_file('${fileStems[0]}') – ${Math.floor(rnd()*500)+80} lines` },
        { t: '16:21:08', text: `Applied edit to ${fileStems[1]} (+${Math.floor(rnd()*12)+1}, -${Math.floor(rnd()*5)})` },
        { t: '16:21:14', text: `✓ Working on task; rerunning checks.` },
      ],
      gitChanges,
      diffStat: {
        add: gitChanges.reduce((s, g) => s + (g.plus  || 0), 0),
        del: gitChanges.reduce((s, g) => s + (g.minus || 0), 0),
      },
      activity,
      todos: { done: [], open: [] },
    };
  }

  function _todosForWorkerFromDom(name) {
    const list = document.getElementById('todos-list');
    if (!list) return { done: [], open: [] };
    const open = [];
    list.querySelectorAll('.todo-item').forEach(item => {
      const assignee = item.querySelector('.todo-assignee');
      const desc     = item.querySelector('.todo-desc');
      const by       = item.querySelector('.todo-by');
      if (!assignee || !desc) return;
      if (assignee.textContent.replace(/^@/, '').trim() !== name) return;
      let from = '', age = '';
      if (by) {
        const parts = by.textContent.split('·').map(s => s.trim());
        const fromPart = parts.find(p => p.startsWith('from '));
        const agePart  = parts.find(p => p.endsWith('ago'));
        if (fromPart) from = fromPart.slice(5);
        if (agePart)  age  = agePart.replace(/\s*ago$/, '');
      }
      open.push({ text: desc.textContent.trim(), from, age });
    });
    return { done: [], open };
  }

  function openWorkerGlance(workerName, workerRole) {
    if (!workerName) return;
    glanceOpenFor = workerName;
    const g = _mockGlance(workerName, workerRole);
    g.todos = _todosForWorkerFromDom(workerName);
    renderWorkerGlance(g);
    const p = document.getElementById('worker-glance');
    if (p) p.hidden = false;
    const a11y = document.getElementById('a11y-panel');
    if (a11y) a11y.hidden = true;
    document.querySelectorAll('.roster-item').forEach(el => {
      el.classList.toggle('active', el.dataset.worker === workerName);
    });
  }

  function closeWorkerGlance() {
    glanceOpenFor = null;
    const p = document.getElementById('worker-glance');
    if (p) p.hidden = true;
    document.querySelectorAll('.roster-item').forEach(el => el.classList.remove('active'));
  }

  function renderWorkerGlance(g) {
    const set = (id, val) => { const el = document.getElementById(id); if (el) el.textContent = val; };
    const avatar = document.getElementById('glance-avatar');
    if (avatar) avatar.textContent = (g.name[0] || '?').toUpperCase();
    set('glance-name', g.name);
    set('glance-role', g.role);
    set('glance-pid',  'PID ' + g.pid);
    set('glance-cwd',  g.cwd);
    set('glance-last-age', g.lastOutputAge);
    set('glance-last-sub', 'since last output');
    set('glance-uncommitted', String(g.gitChanges.length));
    set('glance-cost', g.cost);
    set('glance-calls', g.calls + ' calls');
    set('glance-output-hint', 'from claude · ' + g.lastOutputAge);
    set('glance-git-hint', `${g.branch} · +${g.diffStat.add} −${g.diffStat.del}`);

    const state = document.getElementById('glance-state');
    if (state) {
      state.textContent = '● ' + g.state.toUpperCase();
      state.classList.remove('idle', 'offline');
      if (g.state === 'idle')    state.classList.add('idle');
      if (g.state === 'offline') state.classList.add('offline');
    }

    const out = document.getElementById('glance-output');
    if (out) {
      out.innerHTML = '';
      g.output.forEach(line => {
        const row = document.createElement('div');
        row.className = 'glance-output-line';
        const t = document.createElement('span');
        t.className = 't';
        t.textContent = line.t;
        const txt = document.createElement('span');
        txt.textContent = line.text;
        row.appendChild(t);
        row.appendChild(txt);
        out.appendChild(row);
      });
    }

    const gl = document.getElementById('glance-git-list');
    if (gl) {
      gl.innerHTML = '';
      g.gitChanges.forEach(f => {
        const row = document.createElement('div');
        row.className = 'glance-git-row';
        const tag = document.createElement('span');
        // '.git-tag.?' isn't a legal CSS class — map '?' → 'Q' for the class
        // while keeping the visible glyph as '?'.
        tag.className = 'git-tag ' + (f.tag === '?' ? 'Q' : f.tag);
        tag.textContent = f.tag;
        const path = document.createElement('span');
        path.className = 'glance-git-path';
        path.textContent = f.path;
        row.appendChild(tag);
        row.appendChild(path);
        if (f.plus) {
          const plus = document.createElement('span');
          plus.className = 'glance-git-plus';
          plus.textContent = '+' + f.plus;
          row.appendChild(plus);
        }
        if (f.minus) {
          const minus = document.createElement('span');
          minus.className = 'glance-git-minus';
          minus.textContent = '−' + f.minus;
          row.appendChild(minus);
        }
        gl.appendChild(row);
      });
    }

    const todoEl = document.getElementById('glance-todos');
    if (todoEl) {
      todoEl.innerHTML = '';
      const mkHead = (label) => {
        const h = document.createElement('div');
        h.className = 'glance-todos-heading';
        h.textContent = label;
        return h;
      };
      const mkTodo = (t, done) => {
        const row = document.createElement('div');
        row.className = 'glance-todo' + (done ? ' done' : '');
        const chk = document.createElement('span');
        chk.className = 'glance-todo-check';
        chk.textContent = done ? '✓' : '';
        const body = document.createElement('div');
        body.className = 'glance-todo-body';
        const txt = document.createElement('div');
        txt.className = 'glance-todo-text';
        txt.textContent = t.text;
        body.appendChild(txt);
        if (t.from || t.age) {
          const sub = document.createElement('div');
          sub.className = 'glance-todo-sub';
          if (t.from) {
            sub.appendChild(document.createTextNode('from '));
            const b = document.createElement('b');
            b.textContent = t.from;
            sub.appendChild(b);
            if (t.age) sub.appendChild(document.createTextNode(' · ' + t.age + ' ago'));
          } else if (t.age) {
            sub.textContent = t.age + ' ago';
          }
          body.appendChild(sub);
        }
        row.appendChild(chk);
        row.appendChild(body);
        return row;
      };

      const done = g.todos.done || [];
      const open = g.todos.open || [];
      if (done.length) {
        todoEl.appendChild(mkHead('Last done'));
        todoEl.appendChild(mkTodo(done[0], true));
      }
      todoEl.appendChild(mkHead('Currently assigned'));
      if (!open.length) {
        const empty = document.createElement('div');
        empty.className = 'glance-todo-sub';
        empty.textContent = 'Nothing assigned right now.';
        todoEl.appendChild(empty);
      } else {
        open.forEach(t => todoEl.appendChild(mkTodo(t, false)));
      }
      const hint = document.getElementById('glance-todos-hint');
      if (hint) hint.textContent = `${done.length} done · ${open.length} open`;
    }

    const spark = document.getElementById('glance-sparkline');
    if (spark) {
      spark.innerHTML = '';
      const max = Math.max(...g.activity, 1);
      g.activity.forEach((v, i) => {
        const bar = document.createElement('div');
        const h = Math.max(2, (v / max) * 100);
        bar.className = 'glance-spark-bar'
          + (v === 0 ? ' empty' : (i >= g.activity.length - 3 ? ' active' : ''));
        bar.style.height = h + '%';
        spark.appendChild(bar);
      });
    }
  }

  function wireWorkerGlance() {
    const bind = (id, fn) => {
      const el = document.getElementById(id);
      if (el) el.addEventListener('click', fn);
    };
    bind('glance-close', closeWorkerGlance);
    bind('glance-dm', () => {
      if (!glanceOpenFor) return;
      const target = glanceOpenFor;
      const toEl = document.getElementById('compose-to');
      const ta   = document.getElementById('compose-text');
      if (toEl) toEl.value = target;
      closeWorkerGlance();
      // Visible feedback so the user notices the wiring fired even though
      // the panel closed: flash the compose-to field, focus the textarea,
      // and toast the recipient.
      if (toEl) {
        toEl.classList.add('flash');
        setTimeout(() => toEl.classList.remove('flash'), 900);
      }
      if (ta) ta.focus();
      toast(`Composing to @${target}`);
    });
    bind('glance-logs', () => {
      // Force open + scroll the log into view, so clicking always shows
      // the user something happened (even if the panel was already open).
      if (!prefs.panels.log) toggleServerLog();
      const inner = document.getElementById('server-log-inner');
      if (inner) inner.scrollTop = inner.scrollHeight;
    });
    bind('glance-open-log', () => {
      if (!prefs.panels.log) toggleServerLog();
      const inner = document.getElementById('server-log-inner');
      if (inner) inner.scrollTop = inner.scrollHeight;
      closeWorkerGlance();
    });
    bind('glance-stop', () => {
      toast('Per-worker stop isn\'t wired yet — use "Stop All" for now.', true);
    });

    // Escape closes whichever floating panel is visible.
    document.addEventListener('keydown', (e) => {
      if (e.key !== 'Escape') return;
      const g = document.getElementById('worker-glance');
      if (g && !g.hidden) { closeWorkerGlance(); return; }
      const a = document.getElementById('a11y-panel');
      if (a && !a.hidden) { a.hidden = true; }
    });
  }

  // ── Boot ────────────────────────────────────────────────────
  buildA11yPanel();
  buildWorkerGlance();
  wireA11yPanel();
  wireWorkerGlance();
  reflectPrefsIntoA11yPanel();

  // Expose to core app.js. The core stubs forward to these once loaded.
  window.__panels = {
    toggleA11y: toggleA11yPanel,
    openGlance: openWorkerGlance,
    closeGlance: closeWorkerGlance,
    reflectPrefs: reflectPrefsIntoA11yPanel,
  };
})();
