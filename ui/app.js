// Served by hvac-core's web module, either on the LAN or to the desktop app over loopback.
async function api(path, body) {
  const r = await fetch(`api/${path}`, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!r.ok) throw new Error((await r.text()) || r.statusText);
  return r.status === 204 ? null : r.json();
}

const $ = (id) => document.getElementById(id);

// Setpoint field limits; the unit packs it into 6 bits.
const TEMP_MIN = 0;
const TEMP_MAX = 63;
// In Fahrenheit mode the field holds degF - 64 (confirmed: 10 = 74F, 11 = 75F).
const F_OFFSET = 64;

let snapshot = null;
// Settings the user is editing; sent after a short pause so +/- taps coalesce.
let draft = null;
let sendTimer = null;
// How long the dial's settling animation runs; matches --move in style.css.
const SETTLE_MS = 300;
let settleTimer = null;

function showError(msg) {
  $('error').hidden = !msg;
  $('error').textContent = msg || '';
}

function ago(ms) {
  const s = Math.round((Date.now() - ms) / 1000);
  return s < 60 ? `${s}s ago` : `${Math.round(s / 60)}m ago`;
}

// Slides a new number into a slot, unless it already shows that number.
function swap(slot, value, animate = true) {
  const text = String(value);
  const current = slot.querySelector('span:not(.out)');
  if (current?.textContent === text) return;
  if (current && animate) {
    current.className = 'out';
    current.onanimationend = () => current.remove();
  } else {
    slot.replaceChildren();
  }
  const next = document.createElement('span');
  next.textContent = text;
  if (current && animate) next.className = 'in';
  slot.append(next);
}

// Shows `from` (what the unit has) and `to` (where it's headed) while they differ. Each side
// changes on its own, so a change made mid-flight leaves the arrow in place.
function showSetpoint(from, to) {
  const dial = $('dial');
  if (from !== to) {
    clearTimeout(settleTimer);
    const resting = !dial.classList.contains('pending') && !dial.classList.contains('settling');
    dial.classList.remove('settling');
    dial.classList.add('pending');
    swap($('t-from'), from);
    swap($('t-to'), to, !resting);
    return;
  }
  if (dial.classList.contains('settling')) return;
  if (!dial.classList.contains('pending')) {
    swap($('t-from'), to);
    return;
  }
  // The target stuck: its number takes the centre as the old one and the arrow leave.
  swap($('t-to'), to);
  dial.classList.replace('pending', 'settling');
  settleTimer = setTimeout(() => {
    dial.classList.add('snap');
    dial.classList.remove('settling');
    swap($('t-from'), to, false);
    void dial.offsetWidth;
    dial.classList.remove('snap');
  }, SETTLE_MS);
}

function render() {
  if (!snapshot) return;
  const s = draft || snapshot.settings;
  $('name').textContent = snapshot.name;
  $('dot').classList.toggle('on', snapshot.online);
  let status = snapshot.online ? 'Connected' : 'Offline';
  if (snapshot.lastSeen) status += ` · seen ${ago(snapshot.lastSeen)}`;
  if (snapshot.pending) status += ` · ${snapshot.pending} queued`;
  $('status').textContent = status;

  // Raw sensor value; its scale is not known yet.
  const room = snapshot.props.f_temp_in;
  $('room').textContent = room == null ? '--' : `${room} (raw)`;
  $('raw').textContent = JSON.stringify(snapshot.props, null, 1);

  const controls = document.querySelectorAll('.seg button, .toggles button, .step, #power');
  controls.forEach((b) => (b.disabled = !s));
  if (!s) return;

  const shown = (t) => (s.fahrenheit ? t + F_OFFSET : t);
  showSetpoint(shown((snapshot.reported ?? s).temp), shown(s.temp));
  $('unit').textContent = s.fahrenheit ? '°F' : '°C';
  $('power').classList.toggle('active', s.power);
  $('power').setAttribute('aria-pressed', s.power);
  document.body.className = !s.power ? 'off' : s.mode === 1 ? 'heat' : s.mode === 2 ? 'cool' : '';
  for (const b of $('mode').children) b.classList.toggle('active', +b.dataset.v === s.mode);
  for (const b of $('fan').children) b.classList.toggle('active', +b.dataset.v === s.fan);
  for (const b of document.querySelectorAll('.toggles button')) {
    b.classList.toggle('active', !!s[b.dataset.k]);
  }
}

function edit(change) {
  const base = draft || snapshot?.settings;
  if (!base) return;
  draft = { ...base, ...change };
  render();
  clearTimeout(sendTimer);
  sendTimer = setTimeout(send, 250);
}

async function send() {
  const settings = draft;
  try {
    snapshot = await api('settings', settings);
    showError(null);
  } catch (e) {
    showError(e.message);
  }
  if (draft === settings) draft = null;
  render();
}

$('power').onclick = () => edit({ power: !(draft || snapshot.settings).power });
$('up').onclick = () => edit({ temp: Math.min(TEMP_MAX, (draft || snapshot.settings).temp + 1) });
$('down').onclick = () => edit({ temp: Math.max(TEMP_MIN, (draft || snapshot.settings).temp - 1) });
for (const b of $('mode').children) b.onclick = () => edit({ mode: +b.dataset.v, power: true });
for (const b of $('fan').children) b.onclick = () => edit({ fan: +b.dataset.v });
for (const b of document.querySelectorAll('.toggles button')) {
  b.onclick = () => edit({ [b.dataset.k]: !(draft || snapshot.settings)[b.dataset.k] });
}
$('refresh').onclick = (e) => {
  e.preventDefault();
  api('refresh', {}).catch((err) => showError(err.message));
};

// The stream starts with the current state; EventSource reconnects on its own.
const events = new EventSource('api/events');
events.addEventListener('state', (e) => {
  snapshot = JSON.parse(e.data);
  showError(null);
  render();
});
events.onerror = () => {
  // A plain request surfaces the server's reason (e.g. no device config).
  api('state').then(() => showError('Lost connection to server; retrying…'), (e) => showError(e.message));
};

setInterval(render, 5000);
