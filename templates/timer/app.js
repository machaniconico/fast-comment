const params = new URLSearchParams(window.location.search);
const overlay = document.getElementById('overlay');
const timerCard = document.getElementById('timer');
const labelEl = document.getElementById('label');
const timeEl = document.getElementById('time');
const stateEl = document.getElementById('state');

let snapshot = {
  state: 'idle',
  mode: 'countdown',
  durationSec: 0,
  baseElapsedSec: 0,
  runningSinceMs: 0,
  updatedAt: 0,
};

applyParams();
connect();
window.setInterval(render, 200);
render();

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
  const wsUrl = params.get('ws') || `ws://${window.location.host}/timer`;
  let socket;

  try {
    socket = new WebSocket(wsUrl);
  } catch {
    retry();
    return;
  }

  socket.addEventListener('message', (event) => {
    try {
      const parsed = JSON.parse(event.data);
      if (parsed && typeof parsed === 'object') {
        snapshot = normalizeSnapshot(parsed);
        render();
      }
    } catch {
      // Ignore malformed frames and wait for the next snapshot.
    }
  });
  socket.addEventListener('close', retry);
  socket.addEventListener('error', () => {
    try {
      socket.close();
    } catch {
      // Already closed.
    }
  });
}

function retry() {
  window.setTimeout(connect, 1500);
}

function normalizeSnapshot(value) {
  const mode = value.mode === 'elapsed' ? 'elapsed' : 'countdown';
  const state = typeof value.state === 'string' && value.state ? value.state : 'idle';
  return {
    state,
    mode,
    durationSec: toNonNegativeInt(value.durationSec),
    baseElapsedSec: toNonNegativeInt(value.baseElapsedSec),
    runningSinceMs: toNonNegativeInt(value.runningSinceMs),
    updatedAt: toNonNegativeInt(value.updatedAt),
  };
}

function render() {
  const value = displayValue(snapshot, Date.now());
  const displaySeconds = snapshot.mode === 'countdown'
    ? Math.ceil(value)
    : Math.floor(value);
  const finished =
    snapshot.state === 'finished' ||
    (snapshot.mode === 'countdown' && snapshot.state !== 'idle' && value <= 0);

  labelEl.textContent = snapshot.mode === 'elapsed' ? 'ELAPSED' : 'COUNTDOWN';
  timeEl.textContent = formatDuration(displaySeconds);
  stateEl.textContent = finished ? 'FINISHED' : snapshot.state.toUpperCase();

  timerCard.classList.remove('idle', 'running', 'paused', 'finished');
  if (finished) {
    timerCard.classList.add('finished');
  } else if (snapshot.state === 'running') {
    timerCard.classList.add('running');
  } else if (snapshot.state === 'paused') {
    timerCard.classList.add('paused');
  } else {
    timerCard.classList.add('idle');
  }
}

function displayValue(current, nowMs) {
  const elapsed = elapsedSeconds(current, nowMs);
  if (current.mode === 'elapsed') return elapsed;
  return Math.max(0, current.durationSec - elapsed);
}

function elapsedSeconds(current, nowMs) {
  const base = Math.max(0, Number(current.baseElapsedSec) || 0);
  if (current.state !== 'running') return base;
  const since = Math.max(0, Number(current.runningSinceMs) || 0);
  return base + Math.max(0, nowMs - since) / 1000;
}

function formatDuration(value) {
  const totalSeconds = Math.max(0, Math.floor(Number(value) || 0));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) return `${hours}:${pad2(minutes)}:${pad2(seconds)}`;
  return `${pad2(minutes)}:${pad2(seconds)}`;
}

function pad2(value) {
  return String(value).padStart(2, '0');
}

function toNonNegativeInt(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || n <= 0) return 0;
  return Math.floor(n);
}

function clampNumber(value, fallback, min, max) {
  if (!Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, value));
}
