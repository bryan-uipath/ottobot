// Popover board: polls `snapshot` every 2s while visible; same hierarchy as the TUI board.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);
let visible = false;

// Theme follows the OS.
const dark = matchMedia('(prefers-color-scheme: dark)');
const theme = () => { document.documentElement.dataset.theme = dark.matches ? 'dark' : 'light'; };
dark.addEventListener('change', theme);
theme();

// Element helper: el('span', 'cls', 'text', ...children).
function el(tag, cls, text, ...kids) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  e.append(...kids);
  return e;
}

// Coarse duration, e.g. `49s`, `14m`, `2h11m`, `3d4h`.
function short(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
  return `${Math.floor(s / 86400)}d${Math.floor((s % 86400) / 3600)}h`;
}

// Elapsed wall clock, e.g. `0:42`, `1:02:03`.
function clock(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const mm = String(Math.floor((s % 3600) / 60)), ss = String(s % 60).padStart(2, '0');
  return s >= 3600 ? `${Math.floor(s / 3600)}:${mm.padStart(2, '0')}:${ss}` : `${mm}:${ss}`;
}

// Run duration, e.g. `42s`, `1m02s`, `2h05m`, `1d3h` (TUI `rel_dur`).
function dur(ms) {
  const s = Math.max(0, Math.floor(ms / 1000)), p = (n) => String(n).padStart(2, '0');
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${p(s % 60)}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h${p(Math.floor((s % 3600) / 60))}m`;
  return `${Math.floor(s / 86400)}d${Math.floor((s % 86400) / 3600)}h`;
}

const firstLine = (s) => (s || '').split('\n')[0];
const basename = (p) => p.split('/').filter(Boolean).pop() || p;
const row = (glyph, name, rest, ...extra) => el('div', 'row', null, glyph, el('span', 'name', name), el('span', 'rest', rest), ...extra);
const none = () => el('div', 'none', 'none');

function render(st) {
  const now = Date.now();
  const pw = st.power ? (st.power.on_ac ? ' · AC' : ` · ⚠ battery${st.power.percent != null ? ' ' + st.power.percent + '%' : ''}`) : '';
  $('who').textContent = `pid ${st.daemon_pid ?? '?'} · ${basename(st.root)}${pw}`;
  $('who').classList.toggle('warn', !!st.power && !st.power.on_ac);
  const out = [];

  out.push(el('div', 'label', 'now'));
  const running = st.runs.filter((r) => r.status === 'Running');
  if (!running.length) out.push(none());
  for (const r of running) {
    const rm = r.model ? `${r.runner}/${r.model}` : r.runner;
    out.push(row(el('span', 'spin'), r.skill, `${clock(now - Date.parse(r.started_at))} · ${rm}`));
  }

  // Parse errors first: they never clear on their own.
  out.push(el('div', 'label', 'attention'));
  const attn = [];
  for (const v of st.skills) if (v.trigger.Err) attn.push([v.name, firstLine(v.trigger.Err), null]);
  for (const [p, e] of st.skill_errors) attn.push([basename(p), firstLine(e), null]);
  const since = now - 24 * 3600e3;
  for (const r of st.runs) {
    if (!r.status.Failed || Date.parse(r.started_at) < since) continue;
    const end = r.finished_at ? Date.parse(r.finished_at) : now;
    attn.push([r.skill, firstLine(r.status.Failed), `\u00a0· ${dur(end - Date.parse(r.started_at))}`]);
  }
  if (!attn.length) out.push(none());
  // `<reason> · <duration>`: the reason truncates, the duration never does.
  for (const [name, why, tail] of attn) {
    out.push(el('div', 'row', null, el('span', 'x', '✗'), el('span', 'name', name),
      el('span', 'rest split', null, el('span', 'why', why), tail ? el('span', null, tail) : '')));
  }

  out.push(el('div', 'label', 'quota'));
  if (!st.runner_health.length) out.push(none());
  for (const [name, health] of st.runner_health) {
    const u = st.runner_usage.find(([n]) => n === name)?.[1];
    if (health.Err != null) { out.push(el('div', 'row', null, el('span', 'x', '✗'), el('span', 'name', name), el('span', 'rest err', firstLine(health.Err)))); continue; }
    if (!u) { out.push(el('div', 'row quota', null, el('span', 'name', name), el('span', 'light', '…'))); continue; }
    if (u.Err != null) { out.push(el('div', 'row quota', null, el('span', 'name', name), el('span', 'light', 'unavailable'))); continue; }
    if (!u.Ok.length) { out.push(el('div', 'row quota', null, el('span', 'name', name), el('span', 'light', 'no usage data'))); continue; }
    for (const w of u.Ok) {
      const left = Math.min(100, Math.max(0, 100 - w.used_percent));
      const fill = el('i');
      fill.style.width = `${left}%`;
      out.push(el('div', 'row quota', null,
        el('span', 'name', `${name} ${w.label}`),
        el('span', w.used_percent >= 80 ? 'bar hot' : 'bar', null, fill),
        el('span', w.used_percent >= 80 ? 'rest err' : 'rest', `${left.toFixed(0)}% left`)));
    }
  }

  out.push(el('div', 'label', 'next'));
  const next = [...st.skills].sort((a, b) => (a.next_fire ? Date.parse(a.next_fire) : Infinity) - (b.next_fire ? Date.parse(b.next_fire) : Infinity));
  if (!next.length) out.push(none());
  for (const v of next) {
    const check = v.trigger.Ok?.startsWith('event') ? 'check ' : '';
    const t = v.next_fire && Date.parse(v.next_fire) - now;
    const when = v.next_fire ? check + (t < 0 ? 'now' : `in ${short(t)}`) : 'no schedule';
    const last = v.active_run ? 'running' : v.last_fired ? `last ${short(now - Date.parse(v.last_fired))} ago` : 'never ran';
    // Glyph = latest run outcome.
    const latestRun = st.runs.find(r => r.skill === v.name && r.status !== 'Running');
    const glyph = v.active_run ? el('span', 'spin')
      : v.trigger.Err ? el('span', 'x', '✗')
      : latestRun?.status === 'Succeeded' ? el('span', 'ok', '●')
      : latestRun?.status?.Failed !== undefined ? el('span', 'x', '✗')
      : el('span', 'dot', '○');
    const btn = el('button', null, 'run');
    btn.disabled = !!v.active_run;
    btn.onclick = () => invoke('run_now', { skill: v.name }).then(refresh, refresh);
    out.push(row(glyph, v.name, `${when} · ${last}`, btn));
    // One-line summary of the latest finished run.
    const latest = st.runs.find(r => r.skill === v.name && r.status !== 'Running');
    if (latest?.summary || latest?.report) {
      const sub = el('div', 'sub', latest.summary ? `↳ ${latest.summary}` : '↳ ');
      if (latest.report) {
        const a = el('a', 'report', 'report');
        a.href = '#';
        a.onclick = (e) => { e.preventDefault(); invoke('open_report', { path: latest.report }); };
        sub.append(' · ', a);
      }
      out.push(sub);
    }
  }

  $('body').replaceChildren(...out);
}

// Connected-but-unparseable (old daemon) vs. no daemon at all.
function renderDown(err) {
  const old = String(err).includes('older protocol');
  $('who').textContent = '';
  $('body').replaceChildren(el('div', 'down', null,
    el('div', 'big', old ? 'daemon speaks an older protocol — restart it' : 'daemon not running'),
    old ? '' : el('div', 'de', null, 'start it with ', el('code', null, 'otto daemon --install')),
    el('div', 'why', String(err))));
}

async function refresh() {
  try { render(await invoke('snapshot')); } catch (e) { renderDown(e); }
}

$('skills').onclick = () => invoke('open_skills');
$('quit').onclick = () => invoke('quit');
listen('visible', (e) => { visible = e.payload; if (visible) refresh(); });
setInterval(() => { if (visible) refresh(); }, 2000);
refresh();
