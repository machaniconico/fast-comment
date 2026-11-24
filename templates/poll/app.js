const params = new URLSearchParams(window.location.search);
const poll = document.getElementById('poll');
const question = document.getElementById('question');
const status = document.getElementById('status');
const remaining = document.getElementById('remaining');
const voters = document.getElementById('voters');
const chart = document.getElementById('chart');
const pie = document.getElementById('pie');
const options = document.getElementById('options');

const COLORS = ['#58a6ff', '#f0883e', '#3fb950', '#db61a2', '#a371f7',
  '#e3b341', '#39c5cf', '#f85149', '#8ddb8c', '#bc8cff'];
const SVG = 'http://www.w3.org/2000/svg';

// ---- fc-poll:begin ----
function sector(start, percent) {
  if (percent <= 0) return '';
  if (percent >= 100) return 'M 50 5 A 45 45 0 1 1 50 95 A 45 45 0 1 1 50 5 Z';
  const point = (position) => {
    const angle = position * Math.PI / 50;
    return `${50 + 45 * Math.sin(angle)} ${50 - 45 * Math.cos(angle)}`;
  };
  return `M 50 50 L ${point(start)} A 45 45 0 ${percent > 50 ? 1 : 0} 1 ${point(start + percent)} Z`;
}

function roundPercent(percent) {
  return `${Math.round(percent)}%`;
}

function isWinner(status, leading) {
  return status === 'closed' && leading;
}

function timeLeft(endsAtMs, nowMs) {
  const seconds = Math.max(0, Math.ceil((endsAtMs - nowMs) / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}
// ---- fc-poll:end ----

let endsAtMs;
let reconnectDelay = 1000;

function updateClock() {
  remaining.textContent = endsAtMs == null ? '' : `残り ${timeLeft(endsAtMs, Date.now())}`;
}

function svgElement(tag, attributes) {
  const element = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, value);
  return element;
}

function drawPie(snapshot) {
  const svg = svgElement('svg', { viewBox: '0 0 100 100', 'aria-hidden': 'true' });
  const group = svgElement('g', {});
  if (snapshot.display === 'donut') {
    const mask = svgElement('mask', { id: 'poll-donut-mask' });
    mask.append(svgElement('circle', { cx: 50, cy: 50, r: 46, fill: 'white' }));
    mask.append(svgElement('circle', { cx: 50, cy: 50, r: 25, fill: 'black' }));
    svg.append(mask);
    group.setAttribute('mask', 'url(#poll-donut-mask)');
  }
  group.append(svgElement('circle', { cx: 50, cy: 50, r: 45, fill: '#ffffff33' }));
  let start = 0;
  snapshot.options.forEach((option, index) => {
    const path = sector(start, option.percent);
    if (path) group.append(svgElement('path', { d: path, fill: COLORS[index] }));
    start += option.percent;
  });
  svg.append(group);
  return svg;
}

function drawOption(snapshot, option, index) {
  const row = document.createElement('li');
  row.classList.toggle('winner', isWinner(snapshot.status, option.leading));
  row.style.setProperty('--color', COLORS[index]);
  row.style.setProperty('--percent', `${option.percent}%`);

  const label = document.createElement('span');
  label.className = 'label';
  label.textContent = `${index + 1}. ${option.label}`;

  const values = document.createElement('span');
  values.className = 'values';
  values.textContent = [
    snapshot.showCounts ? `${option.votes}票` : '',
    snapshot.showPercent ? roundPercent(option.percent) : '',
  ].filter(Boolean).join(' · ');

  if (snapshot.display === 'bar-h' || snapshot.display === 'bar-v') {
    const track = document.createElement('div');
    track.className = 'track';
    track.append(document.createElement('div'));
    row.append(track);
  }
  row.append(label, values);
  return row;
}

function render(snapshot) {
  poll.hidden = snapshot.status === 'idle';
  endsAtMs = snapshot.status === 'open' ? snapshot.endsAtMs : undefined;
  if (poll.hidden) return;
  question.textContent = snapshot.question;
  status.textContent = snapshot.status === 'closed' ? '結果' : '投票受付中';
  voters.textContent = `総投票者数 ${snapshot.totalVoters}人`;
  updateClock();
  chart.className = snapshot.display;
  options.replaceChildren(...snapshot.options.map((option, index) => drawOption(snapshot, option, index)));
  const circular = snapshot.display === 'pie' || snapshot.display === 'donut';
  pie.replaceChildren(...(circular ? [drawPie(snapshot)] : []));
}

function retry() {
  window.setTimeout(connect, reconnectDelay);
  reconnectDelay = Math.min(reconnectDelay * 2, 30000);
}

function connect() {
  let socket;
  try {
    socket = new WebSocket(params.get('ws') || `ws://${window.location.host}/poll`);
  } catch {
    retry();
    return;
  }
  let stableTimer;
  socket.addEventListener('open', () => {
    stableTimer = window.setTimeout(() => { reconnectDelay = 1000; }, 5000);
  });
  socket.addEventListener('message', ({ data }) => {
    try {
      render(JSON.parse(data));
    } catch {
      // 次のスナップショットを待つ。
    }
  });
  socket.addEventListener('close', () => {
    window.clearTimeout(stableTimer);
    retry();
  });
  socket.addEventListener('error', () => socket.close());
}

connect();
window.setInterval(updateClock, 250);
