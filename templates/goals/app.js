const params = new URLSearchParams(window.location.search);
const overlay = document.getElementById('overlay');
const goalsRoot = document.getElementById('goals');

const METRICS = [
  { key: 'likes', label: 'LIKES', className: 'likes' },
  { key: 'comments', label: 'COMMENTS', className: 'comments' },
  { key: 'viewers', label: 'VIEWERS', className: 'viewers' },
];

const only = new Set(
  (params.get('only') || '')
    .split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean)
);

let reconnectDelay = 1000;
let stableTimer = null;
let reconnectTimer = null;

applyParams();
connect();

function applyParams() {
  const font = clampNumber(Number(params.get('font')), 100, 50, 200) / 100;
  const bg = clampNumber(Number(params.get('bg')), 82, 0, 100) / 100;
  const pos = (params.get('pos') || 'bottom').toLowerCase();

  document.documentElement.style.setProperty('--font-scale', String(font));
  document.documentElement.style.setProperty('--bg-alpha', bg.toFixed(2));

  overlay.classList.remove('top', 'bottom', 'left', 'right');
  overlay.classList.add(['top', 'bottom', 'left', 'right'].includes(pos) ? pos : 'bottom');
}

function connect() {
  const wsUrl = params.get('ws') || `ws://${window.location.host}/stats`;
  let socket;

  try {
    socket = new WebSocket(wsUrl);
  } catch {
    retry();
    return;
  }

  socket.addEventListener('open', () => {
    stableTimer = window.setTimeout(() => {
      reconnectDelay = 1000;
    }, 5000);
  });

  socket.addEventListener('message', (event) => {
    try {
      render(JSON.parse(event.data));
    } catch {
      // Ignore malformed frames and wait for the next snapshot.
    }
  });
  socket.addEventListener('close', () => {
    window.clearTimeout(stableTimer);
    stableTimer = null;
    retry();
  });
  socket.addEventListener('error', () => {
    window.clearTimeout(reconnectTimer);
    reconnectTimer = null;
    try {
      socket.close();
    } catch {
      // Already closed.
    }
  });
}

function retry() {
  window.clearTimeout(reconnectTimer);
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, reconnectDelay);
  reconnectDelay = Math.min(reconnectDelay * 2, 30000);
}

function render(snapshot) {
  if (!snapshot || typeof snapshot !== 'object') return;
  const goals = snapshot.goals || {};
  const cards = [];

  for (const metric of METRICS) {
    if (only.size > 0 && !only.has(metric.key)) continue;
    if (metric.key === 'likes' && snapshot.likesAvailable === false) continue;

    const target = toCount(goals[metric.key]);
    if (target === 0) continue;

    const current = toCount(snapshot[metric.key]);
    cards.push(createCard(metric, current, target));
  }

  goalsRoot.replaceChildren(...cards);
}

function createCard(metric, current, target) {
  const percent = target > 0 ? Math.floor((current * 100) / target) : 0;
  const clamped = Math.min(100, percent);

  const card = document.createElement('section');
  card.className = `goal-card ${metric.className}`;
  if (percent >= 100) card.classList.add('reached');

  const head = document.createElement('div');
  head.className = 'goal-head';

  const title = document.createElement('div');
  title.className = 'goal-title';
  title.textContent = metric.label;

  const pct = document.createElement('div');
  pct.className = 'goal-percent';
  pct.textContent = `${percent}%`;

  head.append(title, pct);

  const value = document.createElement('div');
  value.className = 'goal-value';

  const currentEl = document.createElement('span');
  currentEl.className = 'goal-current';
  currentEl.textContent = formatCount(current);

  const targetEl = document.createElement('span');
  targetEl.className = 'goal-target';
  targetEl.textContent = `/ ${formatCount(target)}`;

  value.append(currentEl, targetEl);

  const bar = document.createElement('div');
  bar.className = 'goal-bar';
  const fill = document.createElement('div');
  fill.className = 'goal-fill';
  fill.style.width = `${clamped}%`;
  bar.append(fill);

  card.append(head, value, bar);
  return card;
}

function toCount(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || n <= 0) return 0;
  return Math.floor(n);
}

function formatCount(value) {
  return new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 }).format(value);
}

function clampNumber(value, fallback, min, max) {
  if (!Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, value));
}
