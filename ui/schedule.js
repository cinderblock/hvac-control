// The schedule summary on the main page and its editor. Uses api, $, ago, events from app.js.
const MODE_NAMES = { 0: 'Fan', 1: 'Heat', 2: 'Cool', 3: 'Dry', 4: 'Auto' };
const FAN_NAMES = { 0: 'Auto', 6: 'Low', 7: 'Med', 8: 'High' };
const DAY_NAMES = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];

let sched = null;
// The schedule being edited, and the rule in force when the editor opened.
let editing = null;
let activeRule = null;

function el(tag, props, ...kids) {
  const e = Object.assign(document.createElement(tag), props);
  e.append(...kids);
  return e;
}

function describe(t) {
  if (!t.power) return 'Off';
  let s = MODE_NAMES[t.mode];
  if (t.tempF != null) s += ` ${t.tempF}°F`;
  if (t.tempF != null && t.rampMinutes) s += ` over ${t.rampMinutes} min`;
  if (t.fan != null) s += `, fan ${FAN_NAMES[t.fan] ?? t.fan}`;
  return s;
}

function holdEnd(h) {
  if (h.until) {
    const time = new Date(h.until).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
    return h.boundary ? `until the next rule change or ${time}` : `until ${time}`;
  }
  return h.boundary ? 'until the next rule change' : 'until you resume';
}

function clock(ms) {
  return new Date(ms).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

// Runs every second, so it stays apart from renderSchedule.
function renderCountdown() {
  const end = sched?.enabled && sched.hold ? sched.holdEnds : null;
  $('sched-countdown').hidden = !end;
  if (!end) return;
  const left = Math.max(0, Math.ceil((end - Date.now()) / 1000));
  const pad = (n) => String(n).padStart(2, '0');
  const h = Math.floor(left / 3600);
  const m = Math.floor(left / 60) % 60;
  const time = h ? `${h}:${pad(m)}:${pad(left % 60)}` : `${m}:${pad(left % 60)}`;
  $('sched-countdown').textContent = `Schedule takes over in ${time}`;
}

function renderSchedule() {
  if (!sched) return;
  const { decision: d, hold, ramp, weather: w } = sched;
  const rule = d.target ? `${d.name} → ${describe(d.target)}` : 'No rule matches; unit left alone';
  let title = rule;
  if (!sched.enabled) title = 'Scheduler off';
  else if (hold) title = 'Manual hold';
  else if (ramp) title = `${d.name}: ramping ${ramp.fromF}°F → ${ramp.toF}°F`;
  else if (sched.inSync === false) title = `${rule} (applying…)`;
  $('sched-title').textContent = title;

  let why = d.target ? d.reasons.join(' · ') || 'No conditions' : '';
  if (!sched.enabled && d.target) why = `Would run ${rule}`;
  if (sched.enabled && hold) why = `Holding ${holdEnd(hold)}${d.target ? `. Schedule wants ${rule}` : ''}`;
  else if (sched.enabled && ramp) why = `Reaches ${ramp.toF}°F at ${clock(ramp.ends)}`;
  $('sched-why').textContent = why;
  $('sched-resume').hidden = !(sched.enabled && hold);

  let weather = 'Set a location for weather rules';
  if (sched.hasLocation) {
    const parts = [];
    if (w.outsideF != null) parts.push(`Outside ${Math.round(w.outsideF)}°F`);
    if (w.highF != null) parts.push(`High ${Math.round(w.highF)}° · Low ${Math.round(w.lowF)}°`);
    if (w.fetchedAt) parts.push(`updated ${ago(w.fetchedAt * 1000)}`);
    if (w.error) parts.push('refresh failing');
    weather = parts.join(' · ') || 'Waiting for weather…';
  }
  $('sched-weather').textContent = weather;
  renderCountdown();
}

function field(label, control) {
  return el('label', {}, el('span', { textContent: label }), control);
}

function textField(label, obj, key, attrs, empty = null) {
  const input = el('input', { value: obj[key] ?? '', ...attrs });
  input.oninput = () => (obj[key] = input.value || empty);
  return field(label, input);
}

function numberField(label, obj, key, attrs) {
  const input = el('input', { type: 'number', inputMode: 'decimal', value: obj[key] ?? '', ...attrs });
  input.oninput = () => (obj[key] = input.value === '' ? null : +input.value);
  return field(label, input);
}

function select(options, value, onchange) {
  const s = el('select', {}, ...options.map(([v, label]) => el('option', { value: v, textContent: label })));
  s.value = value;
  s.onchange = () => onchange(s.value);
  return s;
}

function checkbox(label, obj, key) {
  const input = el('input', { type: 'checkbox', checked: obj[key] });
  input.onchange = () => (obj[key] = input.checked);
  return el('label', { className: 'check' }, input, label);
}

function dayButtons(when) {
  const buttons = DAY_NAMES.map((name, day) => {
    const b = el('button', { type: 'button', textContent: name[0] + name[1] });
    b.classList.toggle('active', when.days.includes(day));
    b.onclick = () => {
      when.days = when.days.includes(day) ? when.days.filter((d) => d !== day) : [...when.days, day].sort();
      b.classList.toggle('active', when.days.includes(day));
      b.dispatchEvent(new Event('input', { bubbles: true }));
    };
    return b;
  });
  return el('div', { className: 'seg days' }, ...buttons);
}

function ruleSummary(rule) {
  const { days, from, to, dateFrom, dateTo, outsideMin, outsideMax, highMin, highMax, lowMin, lowMax } = rule.when;
  const range = (label, min, max) => {
    if (min != null && max != null) return `${label} ${min}–${max}`;
    if (min != null) return `${label} ≥ ${min}`;
    return max != null ? `${label} ≤ ${max}` : null;
  };
  const parts = [
    days.length ? days.map((d) => DAY_NAMES[d]).join(' ') : null,
    from && to ? `${from}–${to}` : null,
    dateFrom && dateTo ? `${dateFrom} to ${dateTo}` : null,
    range('outside', outsideMin, outsideMax),
    range('high', highMin, highMax),
    range('low', lowMin, lowMax),
  ];
  return `${parts.filter(Boolean).join(' · ') || 'Always'} → ${describe(rule.then)}`;
}

function ruleCard(rule, index) {
  const { when, then } = rule;
  const title = el('b', { textContent: rule.name });
  const summary = el('span', { className: 'hint' });
  const badge = el('span', { className: 'badge', textContent: 'active', hidden: rule !== activeRule });
  const refresh = () => {
    title.textContent = rule.name || 'Unnamed';
    summary.textContent = ruleSummary(rule);
    card.classList.toggle('off', !rule.enabled);
  };

  const action = select(
    [['off', 'Off'], ...Object.entries(MODE_NAMES)],
    then.power ? then.mode : 'off',
    (v) => {
      then.power = v !== 'off';
      if (then.power) then.mode = +v;
    },
  );
  const fan = select(
    [['', 'Keep'], ...Object.entries(FAN_NAMES)],
    then.fan ?? '',
    (v) => (then.fan = v === '' ? null : +v),
  );
  const move = (delta) => {
    const rules = editing.rules;
    rules.splice(index, 1);
    rules.splice(index + delta, 0, rule);
    renderRules();
  };
  const remove = () => {
    editing.rules.splice(index, 1);
    renderRules();
  };

  const card = el(
    'details',
    { className: 'rule', oninput: refresh, onchange: refresh },
    el('summary', {}, title, badge, summary),
    el(
      'div',
      { className: 'fields' },
      textField('Name', rule, 'name', {}, ''),
      checkbox('Enabled', rule, 'enabled'),
      el('div', { className: 'wide' }, el('span', { textContent: 'Days (none = every day)' }), dayButtons(when)),
      textField('From', when, 'from', { type: 'time' }),
      textField('To', when, 'to', { type: 'time' }),
      textField('Dates from', when, 'dateFrom', { placeholder: 'MM-DD' }),
      textField('Dates to', when, 'dateTo', { placeholder: 'MM-DD' }),
      numberField('Outside ≥', when, 'outsideMin'),
      numberField('Outside ≤', when, 'outsideMax'),
      numberField('Today high ≥', when, 'highMin'),
      numberField('Today high ≤', when, 'highMax'),
      numberField('Today low ≥', when, 'lowMin'),
      numberField('Today low ≤', when, 'lowMax'),
      field('Action', action),
      numberField('Setpoint °F', then, 'tempF', { min: 64, max: 127 }),
      numberField('Ramp over (min)', then, 'rampMinutes', { min: 1, max: 1440 }),
      field('Fan', fan),
      el(
        'div',
        { className: 'actions' },
        el('button', { type: 'button', textContent: '▲', disabled: index === 0, onclick: () => move(-1) }),
        el('button', { type: 'button', textContent: '▼', disabled: index === editing.rules.length - 1, onclick: () => move(1) }),
        el('button', { type: 'button', textContent: 'Delete', className: 'danger', onclick: remove }),
      ),
    ),
  );
  refresh();
  return card;
}

function renderRules() {
  $('ed-rules').replaceChildren(...editing.rules.map(ruleCard));
}

function renderLocation() {
  $('ed-place').textContent = editing.location ? editing.location.name : 'Not set; weather rules stay inactive';
  $('ed-clear').hidden = !editing.location;
}

function pickPlace(place) {
  editing.location = place;
  $('ed-places').replaceChildren();
  $('ed-query').value = '';
  renderLocation();
}

async function searchPlace() {
  const query = $('ed-query').value.trim();
  const coords = query.match(/^(-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)$/);
  if (coords) {
    pickPlace({ name: query, latitude: +coords[1], longitude: +coords[2] });
    return;
  }
  if (!query) return;
  try {
    const places = await api(`geocode?q=${encodeURIComponent(query)}`);
    const buttons = places.map((p) => el('button', { type: 'button', textContent: p.name, onclick: () => pickPlace(p) }));
    $('ed-places').replaceChildren(...(buttons.length ? buttons : ['No matches']));
    editorError(null);
  } catch (e) {
    editorError(e.message);
  }
}

function renderHold() {
  const hold = editing.hold;
  const hours = hold.kind === 'fixed' ? hold.hours : hold.maxHours;
  $('ed-hold').value = hold.kind === 'boundary' && hours == null ? 'boundaryOpen' : hold.kind;
  $('ed-hold-hours').value = hours ?? '';
  $('ed-hold-field').hidden = hours == null;
}

function holdFrom(choice, hours) {
  if (choice === 'boundary') return { kind: 'boundary', maxHours: hours };
  if (choice === 'boundaryOpen') return { kind: 'boundary', maxHours: null };
  return choice === 'fixed' ? { kind: 'fixed', hours } : { kind: 'resume' };
}

function editorError(msg) {
  $('ed-error').hidden = !msg;
  $('ed-error').textContent = msg || '';
}

async function openEditor() {
  try {
    editing = await api('schedule');
  } catch (e) {
    showError(e.message);
    return;
  }
  activeRule = editing.rules[sched?.decision.rule] ?? null;
  $('ed-enabled').checked = editing.enabled;
  $('ed-min').value = editing.limits.minF;
  $('ed-max').value = editing.limits.maxF;
  $('ed-quiet').value = editing.quiet;
  renderLocation();
  renderHold();
  renderRules();
  editorError(null);
  $('editor').hidden = false;
  document.body.classList.add('editing');
}

function closeEditor() {
  $('editor').hidden = true;
  document.body.classList.remove('editing');
}

async function saveEditor() {
  editing.enabled = $('ed-enabled').checked;
  editing.limits = { minF: +$('ed-min').value, maxF: +$('ed-max').value };
  editing.quiet = $('ed-quiet').value;
  try {
    await api('schedule', editing);
    closeEditor();
  } catch (e) {
    editorError(e.message);
    $('editor').scrollTop = 0;
  }
}

$('sched-edit').onclick = openEditor;
$('sched-resume').onclick = () => api('schedule/resume', {}).catch((e) => showError(e.message));
$('ed-cancel').onclick = closeEditor;
$('ed-save').onclick = saveEditor;
$('ed-search').onclick = searchPlace;
$('ed-query').onkeydown = (e) => {
  if (e.key === 'Enter') searchPlace();
};
$('ed-clear').onclick = () => {
  editing.location = null;
  renderLocation();
};
$('ed-hold').onchange = () => {
  const hours = +$('ed-hold-hours').value || 4;
  editing.hold = holdFrom($('ed-hold').value, hours);
  renderHold();
};
$('ed-hold-hours').oninput = () => {
  editing.hold = holdFrom($('ed-hold').value, +$('ed-hold-hours').value);
};
$('ed-add').onclick = () => {
  editing.rules.push({ name: 'New rule', enabled: true, when: { days: [] }, then: { power: true, mode: 2, tempF: 76, fan: null } });
  renderRules();
  $('ed-rules').lastChild.open = true;
};

events.addEventListener('schedule', (e) => {
  sched = JSON.parse(e.data);
  renderSchedule();
});
setInterval(renderSchedule, 30000);

setInterval(renderCountdown, 1000);
