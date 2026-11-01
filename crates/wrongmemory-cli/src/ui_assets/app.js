// WrongMemory dashboard (read-only). Every value from the store is inserted as
// text (textContent), never as HTML: memory text is untrusted input.
'use strict';

const $ = (id) => document.getElementById(id);

function el(tag, attrs, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') node.className = v;
    // CSSOM, not a style attribute: the page's CSP forbids inline style attributes.
    else if (k === 'style') node.style.cssText = v;
    else if (k === 'on') for (const [ev, fn] of Object.entries(v)) node.addEventListener(ev, fn);
    else node.setAttribute(k, v === true ? '' : String(v));
  }
  for (const c of children.flat()) {
    if (c === undefined || c === null || c === false) continue;
    node.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

async function api(m, p, a) {
  const q = new URLSearchParams({ m });
  if (p) q.set('p', p);
  if (a) q.set('a', JSON.stringify(a));
  const res = await fetch('/api?' + q.toString(), { credentials: 'same-origin' });
  if (res.status === 401) throw { code: 'UNAUTHORIZED', message: 'Session expired: open the link printed by `wrongmemory ui` again.' };
  const body = await res.json();
  if (!body.ok) throw body.error;
  return body.result;
}

const fmtTime = (ms) => (ms ? new Date(ms).toLocaleString() : '—');
const fmtIso = (s) => (s ? new Date(s).toLocaleString() : '—');
function fmtDuration(ms) {
  const s = Math.floor(ms / 1000);
  if (s < 60) return s + 's';
  const m = Math.floor(s / 60);
  if (m < 60) return m + 'm ' + (s % 60) + 's';
  const h = Math.floor(m / 60);
  if (h < 48) return h + 'h ' + (m % 60) + 'm';
  return Math.floor(h / 24) + 'd ' + (h % 24) + 'h';
}
const short = (id) => (id && id.length > 12 ? id.slice(0, 6) + '…' + id.slice(-4) : id || '');
const badge = (text, kind) => el('span', { class: 'badge ' + (kind || text || '') }, text ?? '—');
const errorBox = (e) => el('p', { class: 'error' }, (e && (e.code ? e.code + ': ' : '') + (e.message || e)) || 'error');

const state = { projects: [], stats: {}, view: 'project', pid: null, tab: 'overview', timers: [] };

function clearTimers() {
  state.timers.forEach(clearInterval);
  state.timers = [];
}

// ─── Daemon status (top bar) ────────────────────────────────────────────────

async function refreshDaemon() {
  const box = $('daemon');
  try {
    const h = await api('daemon.health');
    box.replaceChildren(
      el('span', {}, el('span', { class: 'dot' }), 'daemon running'),
      el('span', {}, 'v' + h.daemonVersion),
      el('span', {}, 'pid ' + h.pid),
      el('span', {}, 'up ' + fmtDuration(h.uptimeMs)),
      el('span', {}, h.projects + ' projects'),
      el('span', {}, h.openStores + ' open stores'),
      el('span', {}, h.connections + ' connections'),
    );
    state.health = h;
  } catch (e) {
    box.replaceChildren(el('span', {}, el('span', { class: 'dot down' }), 'daemon unreachable: ' + (e.message || e)));
  }
}

// ─── Sidebar ────────────────────────────────────────────────────────────────

async function loadProjects() {
  const r = await api('project.list');
  state.projects = r.projects || [];
  $('project-count').textContent = state.projects.length;
  await Promise.all(
    state.projects.map(async (p) => {
      try {
        state.stats[p.projectId] = await api('memory.stats', p.projectId);
      } catch (e) {
        state.stats[p.projectId] = { error: e };
      }
    }),
  );
  renderSidebar();
}

const total = (byStatus) => Object.values(byStatus || {}).reduce((a, b) => a + b, 0);

function renderSidebar() {
  const list = $('projects');
  if (!state.projects.length) {
    list.replaceChildren(el('li', { class: 'muted', style: 'padding:8px 10px' }, 'No projects registered.'));
    return;
  }
  list.replaceChildren(
    ...state.projects.map((p) => {
      const st = state.stats[p.projectId] || {};
      const active = state.view === 'project' && state.pid === p.projectId;
      return el(
        'li',
        {},
        el(
          'button',
          { type: 'button', class: 'nav-item' + (active ? ' active' : ''), on: { click: () => openProject(p.projectId) } },
          el('span', { class: 'name', title: p.root }, p.displayName || p.projectId),
          el(
            'span',
            { class: 'sub' },
            st.error ? 'unavailable' : (st.byStatus?.active ?? 0) + ' active / ' + total(st.byStatus) + ' total',
            p.admission === 'read-write' ? null : el('span', {}, p.admission),
          ),
        ),
      );
    }),
  );
  document.querySelectorAll('[data-view]').forEach((b) => {
    b.classList.toggle('active', state.view === b.dataset.view);
  });
}

// ─── Project view ───────────────────────────────────────────────────────────

const TABS = [
  ['overview', 'Overview'],
  ['memories', 'Memories'],
  ['candidates', 'Candidates'],
  ['activity', 'Live activity'],
  ['audit', 'Audit trail'],
  ['health', 'Health check'],
];

function openProject(pid, tab) {
  clearTimers();
  state.view = 'project';
  state.pid = pid;
  state.tab = tab || 'overview';
  renderSidebar();
  const p = state.projects.find((x) => x.projectId === pid);
  const body = el('div', {});
  $('main').replaceChildren(
    el('h1', {}, p.displayName || p.projectId),
    el('div', { class: 'muted mono path' }, p.projectId + ' · ' + p.root),
    el(
      'div',
      { class: 'tabs', role: 'tablist' },
      TABS.map(([id, label]) =>
        el('button', { type: 'button', class: 'tab' + (state.tab === id ? ' active' : ''), on: { click: () => openProject(pid, id) } }, label),
      ),
    ),
    body,
  );
  const render = { overview, memories, candidates, activity, audit, health }[state.tab];
  render(p, body).catch((e) => body.replaceChildren(errorBox(e)));
}

function bars(map) {
  const entries = Object.entries(map || {}).filter(([, n]) => n > 0).sort((a, b) => b[1] - a[1]);
  if (!entries.length) return el('p', { class: 'muted' }, 'Nothing yet.');
  const max = Math.max(...entries.map(([, n]) => n));
  return el(
    'div',
    { class: 'bars' },
    entries.map(([k, n]) =>
      el('div', { class: 'bar' }, el('span', {}, k), el('span', { class: 'track' }, el('span', { class: 'fill', style: 'width:' + (100 * n) / max + '%' })), el('span', { class: 'n' }, n)),
    ),
  );
}

const kv = (pairs) => el('dl', { class: 'kv' }, pairs.flatMap(([k, v]) => [el('dt', {}, k), el('dd', {}, v ?? '—')]));

async function overview(p, body) {
  const [stats, sched, sem] = await Promise.all([
    api('memory.stats', p.projectId),
    api('maintenance.status', p.projectId).catch((e) => ({ error: e })),
    api('semantic.status', p.projectId).catch((e) => ({ error: e })),
  ]);
  state.stats[p.projectId] = stats;
  const s = sched.schedule;
  body.replaceChildren(
    el(
      'div',
      { class: 'cards' },
      [
        ['Memories', total(stats.byStatus)],
        ['Active', stats.byStatus?.active ?? 0],
        ['Pending candidates', stats.pendingCandidates ?? 0],
        ['Graph edges', stats.edges ?? 0],
      ].map(([label, value]) => el('div', { class: 'card' }, el('div', { class: 'label' }, label), el('div', { class: 'value' }, value))),
    ),
    el(
      'div',
      { class: 'grid2' },
      el('div', {}, el('h2', {}, 'By status'), el('div', { class: 'panel' }, bars(stats.byStatus))),
      el('div', {}, el('h2', {}, 'By kind'), el('div', { class: 'panel' }, bars(stats.byKind))),
    ),
    el(
      'div',
      { class: 'grid2' },
      el(
        'div',
        {},
        el('h2', {}, 'Store'),
        el(
          'div',
          { class: 'panel' },
          kv([
            ['Kind', p.storeKind],
            ['Admission', badge(p.admission)],
            ['Reason', p.admissionReason],
            ['Store file', el('span', { class: 'mono path' }, p.storePath)],
            ['Owner epoch', p.ownerEpoch],
            ['Registered', fmtTime(p.createdAtMs)],
          ]),
        ),
      ),
      el(
        'div',
        {},
        el('h2', {}, 'Maintenance & semantic recall'),
        el(
          'div',
          { class: 'panel' },
          sched.error
            ? el('p', { class: 'muted' }, 'Maintenance: ' + (sched.error.message || 'unavailable'))
            : kv([
                ['Schedule', s ? (s.enabled ? 'enabled, every ' + fmtDuration(s.cadenceMs) : 'disabled') : 'none'],
                ['Next run', s ? fmtTime(s.nextDueMs) : '—'],
                ['Last run', s ? fmtTime(s.lastRunMs) + (s.lastOutcome ? ' (' + s.lastOutcome + ')' : '') : '—'],
                ['Semantic provider', sem.error ? '—' : sem.provider + (sem.indexed !== undefined ? ' · ' + sem.indexed + ' indexed' : '')],
              ]),
        ),
      ),
    ),
    el('h2', {}, 'Recent jobs'),
    jobsTable((sched.recentJobs || []).slice(0, 10)),
  );
  renderSidebar();
}

function jobsTable(jobs) {
  if (!jobs.length) return el('p', { class: 'muted' }, 'No jobs yet.');
  return el(
    'table',
    {},
    el('thead', {}, el('tr', {}, ['Job', 'Kind', 'State', 'Created', 'Finished'].map((h) => el('th', {}, h)))),
    el(
      'tbody',
      {},
      jobs.map((j) => el('tr', {}, el('td', { class: 'mono' }, short(j.jobId)), el('td', {}, j.kind), el('td', {}, badge(j.state, j.state === 'succeeded' ? 'ok' : j.state === 'failed' ? 'fail' : '')), el('td', {}, fmtTime(j.createdAtMs)), el('td', {}, fmtTime(j.finishedAtMs)))),
    ),
  );
}

const STATUSES = ['active', 'stale', 'archived', 'superseded', 'contradicted', 'deleted'];

async function memories(p, body) {
  const search = el('input', { type: 'search', placeholder: 'Search memories…', 'aria-label': 'Search memories' });
  const status = el('select', { 'aria-label': 'Status' }, el('option', { value: '' }, 'Active (default)'), el('option', { value: 'all' }, 'All statuses'), STATUSES.map((s) => el('option', { value: s }, s)));
  const kinds = Object.keys(state.stats[p.projectId]?.byKind || {}).sort();
  const kind = el('select', { 'aria-label': 'Kind' }, el('option', { value: '' }, 'All kinds'), kinds.map((k) => el('option', { value: k }, k)));
  const table = el('div', {});
  const more = el('button', { type: 'button', class: 'btn', hidden: true }, 'Load more');
  const info = el('span', { class: 'muted' });
  let cursor = null;
  let rows = [];

  const statuses = () => (status.value === 'all' ? STATUSES : status.value ? [status.value] : undefined);

  async function load(reset) {
    if (reset) {
      cursor = null;
      rows = [];
    }
    try {
      const q = search.value.trim();
      if (q) {
        const r = await api('memory.search', p.projectId, { query: q, limit: 50, statuses: statuses(), kinds: kind.value ? [kind.value] : undefined });
        rows = r.hits.map((h) => h.memory);
        cursor = null;
        info.textContent = rows.length + ' matches' + (r.truncated ? ' (first 50)' : '');
      } else {
        const r = await api('memory.listPage', p.projectId, { limit: 50, cursor: cursor || undefined, statuses: statuses(), kind: kind.value || undefined });
        rows = rows.concat(r.memories);
        cursor = r.nextCursor || null;
        info.textContent = 'Showing ' + rows.length + ' of ' + r.total;
      }
      more.hidden = !cursor;
      table.replaceChildren(memoryTable(p, rows));
    } catch (e) {
      table.replaceChildren(errorBox(e));
    }
  }
  let t;
  search.addEventListener('input', () => {
    clearTimeout(t);
    t = setTimeout(() => load(true), 250);
  });
  status.addEventListener('change', () => load(true));
  kind.addEventListener('change', () => load(true));
  more.addEventListener('click', () => load(false));
  body.replaceChildren(
    el('div', { class: 'toolbar' }, search, status, kind, info),
    table,
    el('div', { class: 'toolbar', style: 'margin-top:12px' }, more),
    el('p', { class: 'note' }, 'Session-private memories are not listed: this view has no session.'),
  );
  await load(true);
}

function memoryTable(p, rows) {
  if (!rows.length) return el('p', { class: 'muted' }, 'No memories.');
  return el(
    'table',
    {},
    el('thead', {}, el('tr', {}, ['Text', 'Kind', 'Status', 'Rev', 'Importance', 'Updated'].map((h) => el('th', {}, h)))),
    el(
      'tbody',
      {},
      rows.map((m) =>
        el(
          'tr',
          { class: 'click', tabindex: 0, on: { click: () => openMemory(p, m.id), keydown: (e) => e.key === 'Enter' && openMemory(p, m.id) } },
          el('td', { class: 'text' }, el('div', { class: 'ellipsis' }, m.text)),
          el('td', {}, m.kind),
          el('td', {}, badge(m.status)),
          el('td', {}, m.revision),
          el('td', {}, typeof m.importance === 'number' ? m.importance.toFixed(2) : '—'),
          el('td', {}, fmtIso(m.updatedAt)),
        ),
      ),
    ),
  );
}

async function openMemory(p, id) {
  const drawer = $('drawer');
  const bodyEl = $('drawer-body');
  $('drawer-title').textContent = 'Memory ' + short(id);
  bodyEl.replaceChildren(el('p', { class: 'muted' }, 'Loading…'));
  drawer.hidden = false;
  try {
    const m = await api('memory.get', p.projectId, { id });
    const anchors = (m.anchors || []).map((a) => [a.type, a.path, a.symbol, a.command].filter(Boolean).join(' '));
    bodyEl.replaceChildren(
      el('p', { class: 'memtext' }, m.text),
      kv([
        ['Id', el('span', { class: 'mono' }, m.id)],
        ['Kind', m.kind],
        ['Status', badge(m.status)],
        ['Scope', m.scope],
        ['Revision', m.revision],
        ['Importance', m.importance],
        ['Confidence', m.confidence],
        ['Tags', (m.tags || []).join(', ') || '—'],
        ['Anchors', anchors.length ? el('div', {}, anchors.map((a) => el('div', { class: 'mono' }, a))) : '—'],
        ['Sources', (m.sources || []).map((s) => s.type).join(', ') || '—'],
        ['Created', fmtIso(m.createdAt)],
        ['Updated', fmtIso(m.updatedAt)],
        ['Superseded by', m.supersededBy],
        ['Stale reason', m.staleReason],
      ]),
      el('h2', {}, 'Full record'),
      el('pre', { class: 'json' }, JSON.stringify(m, null, 2)),
    );
  } catch (e) {
    bodyEl.replaceChildren(errorBox(e));
  }
}

async function candidates(p, body) {
  const status = el('select', { 'aria-label': 'Candidate status' }, ['pending', 'accepted', 'rejected', 'merged'].map((s) => el('option', { value: s }, s)));
  const out = el('div', {});
  async function load() {
    try {
      const r = await api('candidate.list', p.projectId, { status: status.value, limit: 200 });
      const list = r.candidates || [];
      out.replaceChildren(
        list.length
          ? el(
              'table',
              {},
              el('thead', {}, el('tr', {}, ['Proposal', 'Kind', 'Action', 'Status', 'Created'].map((h) => el('th', {}, h)))),
              el('tbody', {}, list.map((c) => el('tr', {}, el('td', { class: 'text' }, el('div', { class: 'ellipsis' }, c.text)), el('td', {}, c.kind), el('td', {}, c.suggestedAction || '—'), el('td', {}, badge(c.status)), el('td', {}, fmtIso(c.createdAt))))),
            )
          : el('p', { class: 'muted' }, 'No ' + status.value + ' candidates.'),
      );
    } catch (e) {
      out.replaceChildren(errorBox(e));
    }
  }
  status.addEventListener('change', load);
  body.replaceChildren(el('div', { class: 'toolbar' }, status), out);
  await load();
}

async function activity(p, body) {
  const live = el('span', { class: 'live' }, el('span', { class: 'dot' }), 'live, refreshes every 3 s');
  const list = el('div', {});
  const events = [];
  let gen = 0;
  let instance;
  async function poll() {
    try {
      const r = await api('events.since', p.projectId, { generation: gen, daemonInstanceId: instance });
      if (r.daemonInstanceId !== instance || r.resync) events.length = 0;
      instance = r.daemonInstanceId;
      gen = r.generation;
      events.unshift(...r.events.slice().reverse());
      events.length = Math.min(events.length, 300);
      list.replaceChildren(
        events.length
          ? el(
              'table',
              {},
              el('thead', {}, el('tr', {}, ['Time', 'Change', 'Memory', 'Rev', 'Type'].map((h) => el('th', {}, h)))),
              el('tbody', {}, events.map((e) => el('tr', {}, el('td', {}, fmtTime(e.atMs)), el('td', { class: 'mono' }, e.change), el('td', { class: 'mono' }, short(e.memoryId || e.candidateId)), el('td', {}, e.revision ?? '—'), el('td', {}, e.telemetry ? 'usage' : e.origin ? 'client' : 'daemon')))),
            )
          : el('p', { class: 'muted' }, 'No changes since this page opened (the daemon keeps a short in-memory history). Changes appear here as clients write.'),
      );
    } catch (e) {
      list.replaceChildren(errorBox(e));
    }
  }
  body.replaceChildren(el('div', { class: 'toolbar' }, live), list);
  await poll();
  state.timers.push(setInterval(poll, 3000));
}

async function audit(p, body) {
  const r = await api('memory.audit', p.projectId, { limit: 300 });
  const recs = r.records || [];
  body.replaceChildren(
    recs.length
      ? el(
          'table',
          {},
          el('thead', {}, el('tr', {}, ['Time', 'Event', 'Memory', 'Reason'].map((h) => el('th', {}, h)))),
          el('tbody', {}, recs.map((a) => el('tr', {}, el('td', {}, fmtIso(a.at || a.createdAt)), el('td', { class: 'mono' }, a.event), el('td', { class: 'mono' }, short(a.memoryId)), el('td', { class: 'text' }, el('div', { class: 'ellipsis' }, a.reason || ''))))),
        )
      : el('p', { class: 'muted' }, 'The audit trail is empty.'),
    el('p', { class: 'note' }, 'Durable record of lifecycle changes (deletes, recoveries, merges, reviews) kept in the store.'),
  );
}

async function health(p, body) {
  const r = await api('memory.doctor', p.projectId);
  const checks = r.checks || [];
  body.replaceChildren(
    el('p', { class: 'muted' }, 'Report only: nothing is repaired from this page. Generated ' + fmtIso(r.generatedAt) + '.'),
    el(
      'table',
      {},
      el('thead', {}, el('tr', {}, ['Check', 'Status', 'Detail'].map((h) => el('th', {}, h)))),
      el('tbody', {}, checks.map((c) => el('tr', {}, el('td', {}, c.title || c.id, el('div', { class: 'muted mono' }, c.id)), el('td', {}, badge(c.severity)), el('td', { class: 'text' }, c.summary || '')))),
    ),
    el('h2', {}, 'Metrics'),
    el('pre', { class: 'json' }, JSON.stringify(r.metrics, null, 2)),
  );
}

// ─── Service views ──────────────────────────────────────────────────────────

async function showGrants() {
  clearTimers();
  state.view = 'grants';
  renderSidebar();
  const main = $('main');
  main.replaceChildren(el('h1', {}, 'Client grants'), el('p', { class: 'muted' }, 'Credentials issued to MCP clients. Secrets are never shown.'));
  try {
    const r = await api('grant.list');
    const name = (id) => state.projects.find((p) => p.projectId === id)?.displayName || id;
    const grants = r.grants || [];
    main.append(
      grants.length
        ? el(
            'table',
            {},
            el('thead', {}, el('tr', {}, ['Label', 'Profile', 'Projects', 'Created', 'State'].map((h) => el('th', {}, h)))),
            el(
              'tbody',
              {},
              grants.map((g) =>
                el('tr', {}, el('td', {}, g.label, el('div', { class: 'muted mono' }, g.grantId)), el('td', {}, badge(g.profile)), el('td', {}, g.projects.map(name).join(', ')), el('td', {}, fmtTime(g.createdAtMs)), el('td', {}, g.revokedAtMs ? badge('revoked', 'fail') : g.expiresAtMs && g.expiresAtMs < Date.now() ? badge('expired', 'warn') : badge('active', 'ok'))),
              ),
            ),
          )
        : el('p', { class: 'muted' }, 'No grants issued.'),
    );
  } catch (e) {
    main.append(errorBox(e));
  }
}

async function showDaemon() {
  clearTimers();
  state.view = 'daemon';
  renderSidebar();
  await refreshDaemon();
  const h = state.health || {};
  $('main').replaceChildren(
    el('h1', {}, 'Daemon'),
    el(
      'div',
      { class: 'panel' },
      kv([
        ['Instance', el('span', { class: 'mono' }, h.daemonInstanceId)],
        ['Version', h.daemonVersion],
        ['Process id', h.pid],
        ['Uptime', h.uptimeMs !== undefined ? fmtDuration(h.uptimeMs) : '—'],
        ['Data root', el('span', { class: 'mono' }, h.dataRoot)],
        ['SQLite', h.sqliteVersion],
        ['Registered projects', h.projects],
        ['Open stores', h.openStores],
        ['Connections', h.connections],
        ['Draining', h.draining ? 'yes' : 'no'],
      ]),
    ),
  );
}

// ─── Boot ───────────────────────────────────────────────────────────────────

document.querySelectorAll('[data-view]').forEach((b) =>
  b.addEventListener('click', () => (b.dataset.view === 'grants' ? showGrants() : showDaemon())),
);
$('drawer-close').addEventListener('click', () => ($('drawer').hidden = true));
document.addEventListener('keydown', (e) => e.key === 'Escape' && ($('drawer').hidden = true));

(async function boot() {
  await refreshDaemon();
  setInterval(refreshDaemon, 5000);
  try {
    await loadProjects();
  } catch (e) {
    $('main').replaceChildren(errorBox(e));
    return;
  }
  if (state.projects.length) openProject(state.projects[0].projectId);
  else $('main').replaceChildren(el('h1', {}, 'No projects yet'), el('p', { class: 'muted' }, 'Register one with `wrongmemory project add <root>`.'));
  setInterval(() => loadProjects().catch(() => {}), 15000);
})();
