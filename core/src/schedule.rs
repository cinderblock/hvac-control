//! Schedule rules, their evaluation and manual-hold tracking. Pure: no I/O and no clock.

use chrono::{Datelike, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};

use crate::control::Settings;
use crate::weather::Location;

/// In Fahrenheit mode the setpoint field holds degF - 64, in 6 bits.
const F_OFFSET: i32 = 64;
const F_MAX: i32 = F_OFFSET + 63;
const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
/// An active rule's weather bounds are relaxed by this much, so a reading near a bound can't flap.
const HYSTERESIS_F: f64 = 1.0;
/// How long after a command the readback may still show the old or the commanded state.
const GRACE_MS: u64 = 5 * 60_000;
/// The link drops commands, so an unconfirmed one is sent again after this long.
const RESEND_MS: u64 = 2 * 60_000;
const MAX_RAMP_MINUTES: u32 = 1440;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub enabled: bool,
    pub location: Option<Location>,
    pub limits: Limits,
    pub hold: Hold,
    #[serde(default)]
    pub quiet: Quiet,
    /// First match wins.
    pub rules: Vec<Rule>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub min_f: i32,
    pub max_f: i32,
}

/// Which scheduled changes mute the unit's beep.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Quiet {
    Off,
    Ramps,
    #[default]
    All,
}

/// How long a manual change keeps the schedule off the unit.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Hold {
    /// Until the active rule changes, or `max_hours`, whichever comes first.
    Boundary { max_hours: Option<f64> },
    Fixed { hours: f64 },
    Resume,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub name: String,
    pub enabled: bool,
    pub when: When,
    pub then: Action,
}

/// All conditions must hold; a condition left `None` always does.
/// A condition on weather we don't have is false, so such a rule stays inactive.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct When {
    /// 0 = Sunday. Empty means every day. An overnight window belongs to the day it starts on.
    pub days: Vec<u8>,
    /// "HH:MM", start inclusive, end exclusive; may wrap past midnight.
    pub from: Option<String>,
    pub to: Option<String>,
    /// "MM-DD", both inclusive; may wrap past new year.
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub outside_min: Option<f64>,
    pub outside_max: Option<f64>,
    /// Today's forecast high.
    pub high_min: Option<f64>,
    pub high_max: Option<f64>,
    /// Today's forecast low.
    pub low_min: Option<f64>,
    pub low_max: Option<f64>,
}

/// `temp_f` and `fan` left `None` keep whatever the unit has. Only `power` applies when off.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    pub power: bool,
    pub mode: u32,
    pub temp_f: Option<i32>,
    pub fan: Option<u32>,
    /// Ease a running unit to `temp_f` over this long instead of jumping.
    pub ramp_minutes: Option<u32>,
}

impl Action {
    pub fn matches(&self, s: &Settings) -> bool {
        if !self.power {
            return !s.power;
        }
        s.power
            && s.mode == self.mode
            && self.temp_f.is_none_or(|t| raw_temp(t, s.fahrenheit) == s.temp)
            && self.fan.is_none_or(|f| f == s.fan)
    }

    /// `current` moved to this action; fields it doesn't manage are left alone.
    pub fn apply(&self, current: &Settings) -> Settings {
        let mut want = *current;
        want.power = self.power;
        if self.power {
            want.mode = self.mode;
            want.temp = self.temp_f.map_or(want.temp, |t| raw_temp(t, want.fahrenheit));
            want.fan = self.fan.unwrap_or(want.fan);
        }
        want
    }
}

fn raw_temp(temp_f: i32, fahrenheit: bool) -> u32 {
    if fahrenheit {
        (temp_f - F_OFFSET).max(0) as u32
    } else {
        ((temp_f - 32) as f64 * 5.0 / 9.0).round().max(0.0) as u32
    }
}

/// The Fahrenheit setpoint the unit is at.
fn temp_f(s: &Settings) -> i32 {
    if s.fahrenheit {
        s.temp as i32 + F_OFFSET
    } else {
        (s.temp as f64 * 9.0 / 5.0 + 32.0).round() as i32
    }
}

pub struct Inputs {
    /// Local wall-clock time.
    pub now: NaiveDateTime,
    pub outside_f: Option<f64>,
    pub high_f: Option<f64>,
    pub low_f: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    /// Index into `rules`.
    pub rule: Option<usize>,
    pub name: Option<String>,
    pub target: Option<Action>,
    /// Why this rule matched, for the UI.
    pub reasons: Vec<String>,
}

fn parse_time(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

fn parse_date(s: &str) -> Option<(u32, u32)> {
    let (m, d) = s.split_once('-')?;
    let (m, d): (u32, u32) = (m.parse().ok()?, d.parse().ok()?);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((m, d))
}

fn bound(label: &str, value: Option<f64>, min: Option<f64>, max: Option<f64>, slack: f64, why: &mut Vec<String>) -> bool {
    if min.is_none() && max.is_none() {
        return true;
    }
    let Some(v) = value else { return false };
    if min.is_some_and(|m| v < m - slack) || max.is_some_and(|m| v > m + slack) {
        return false;
    }
    let range = match (min, max) {
        (Some(a), Some(b)) => format!("{a:.0}–{b:.0}°F"),
        (Some(a), None) => format!("≥ {a:.0}°F"),
        (None, Some(b)) => format!("≤ {b:.0}°F"),
        (None, None) => unreachable!(),
    };
    why.push(format!("{label} {v:.0}°F ({range})"));
    true
}

impl When {
    /// The reasons this matches, or `None` if it doesn't.
    fn check(&self, i: &Inputs, slack: f64) -> Option<Vec<String>> {
        let mut why = Vec::new();
        let minute = i.now.hour() * 60 + i.now.minute();
        let mut date = i.now.date();
        match (&self.from, &self.to) {
            (None, None) => {}
            (Some(from), Some(to)) => {
                let (f, t) = (parse_time(from)?, parse_time(to)?);
                let inside = if f <= t { f <= minute && minute < t } else { minute >= f || minute < t };
                if !inside {
                    return None;
                }
                if f > t && minute < t {
                    date = date.pred_opt()?;
                }
                why.push(format!("{from}–{to}"));
            }
            _ => return None,
        }
        if !self.days.is_empty() {
            let day = date.weekday().num_days_from_sunday() as u8;
            if !self.days.contains(&day) {
                return None;
            }
            why.push(self.days.iter().filter_map(|&d| DAYS.get(d as usize).copied()).collect::<Vec<_>>().join(", "));
        }
        match (&self.date_from, &self.date_to) {
            (None, None) => {}
            (Some(from), Some(to)) => {
                let (f, t, v) = (parse_date(from)?, parse_date(to)?, (date.month(), date.day()));
                let inside = if f <= t { f <= v && v <= t } else { v >= f || v <= t };
                if !inside {
                    return None;
                }
                why.push(format!("{from} to {to}"));
            }
            _ => return None,
        }
        let bounds = [
            ("outside", i.outside_f, self.outside_min, self.outside_max),
            ("forecast high", i.high_f, self.high_min, self.high_max),
            ("forecast low", i.low_f, self.low_min, self.low_max),
        ];
        for (label, value, min, max) in bounds {
            if !bound(label, value, min, max, slack, &mut why) {
                return None;
            }
        }
        Some(why)
    }
}

/// Picks the first enabled rule that matches. `active` is the rule currently in force.
pub fn evaluate(schedule: &Schedule, inputs: &Inputs, active: Option<usize>) -> Decision {
    for (index, rule) in schedule.rules.iter().enumerate().filter(|(_, r)| r.enabled) {
        let slack = if active == Some(index) { HYSTERESIS_F } else { 0.0 };
        let Some(mut reasons) = rule.when.check(inputs, slack) else { continue };
        let mut target = rule.then;
        if !target.power || target.mode == 0 {
            target.temp_f = None;
        }
        if let Some(t) = target.temp_f {
            let limited = t.clamp(schedule.limits.min_f, schedule.limits.max_f);
            if limited != t {
                reasons.push(format!("{t}°F limited to {limited}°F"));
            }
            target.temp_f = Some(limited);
        }
        return Decision { rule: Some(index), name: Some(rule.name.clone()), target: Some(target), reasons };
    }
    Decision::default()
}

/// Minutes until the rule in force stops being `active`, looking `horizon` minutes ahead.
/// `inputs` gives the conditions that many minutes from now.
pub fn next_change(schedule: &Schedule, active: Option<usize>, horizon: u32, inputs: impl Fn(u32) -> Inputs) -> Option<u32> {
    (1..=horizon).find(|&m| evaluate(schedule, &inputs(m), active).rule != active)
}

/// Checks a schedule received from the UI.
pub fn validate(s: &Schedule) -> Result<(), String> {
    let Limits { min_f, max_f } = s.limits;
    if min_f < F_OFFSET || max_f > F_MAX || min_f > max_f {
        return Err(format!("limits must satisfy {F_OFFSET} ≤ min ≤ max ≤ {F_MAX}"));
    }
    match s.hold {
        Hold::Boundary { max_hours: Some(h) } | Hold::Fixed { hours: h } if h <= 0.0 => {
            return Err("hold hours must be positive".into())
        }
        _ => {}
    }
    if let Some(l) = &s.location {
        if l.latitude.abs() > 90.0 || l.longitude.abs() > 180.0 {
            return Err("location is out of range".into());
        }
    }
    for r in &s.rules {
        let fail = |what: &str| format!("rule {:?}: {what}", r.name);
        let w = &r.when;
        if r.name.trim().is_empty() {
            return Err("every rule needs a name".into());
        }
        if w.days.iter().any(|&d| d > 6) {
            return Err(fail("bad day"));
        }
        match (&w.from, &w.to) {
            (None, None) => {}
            (Some(f), Some(t)) => match (parse_time(f), parse_time(t)) {
                (Some(f), Some(t)) if f != t => {}
                _ => return Err(fail("times must be different HH:MM values")),
            },
            _ => return Err(fail("set both times or neither")),
        }
        match (&w.date_from, &w.date_to) {
            (None, None) => {}
            (Some(f), Some(t)) if parse_date(f).is_some() && parse_date(t).is_some() => {}
            _ => return Err(fail("set both dates as MM-DD, or neither")),
        }
        if r.then.mode > 4 {
            return Err(fail("bad mode"));
        }
        if r.then.fan.is_some_and(|f| f != 0 && !(5..=9).contains(&f)) {
            return Err(fail("bad fan speed"));
        }
        if r.then.temp_f.is_some_and(|t| !(F_OFFSET..=F_MAX).contains(&t)) {
            return Err(fail("temperature out of range"));
        }
        if r.then.ramp_minutes.is_some_and(|m| !(1..=MAX_RAMP_MINUTES).contains(&m)) {
            return Err(fail("ramp must be 1 to 1440 minutes"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Managed {
    power: bool,
    mode: u32,
    temp: u32,
    fan: u32,
}

impl Managed {
    fn of(s: &Settings) -> Self {
        Self { power: s.power, mode: s.mode, temp: s.temp, fan: s.fan }
    }
}

/// The last command, and the states its readback may legitimately show for a while.
#[derive(Clone, Copy)]
struct Sent {
    at: u64,
    to: Managed,
    from: Managed,
    /// What the command before this one asked for, if that may still be arriving.
    prior: Option<Managed>,
}

impl Sent {
    fn explains(&self, seen: Managed, now: u64) -> bool {
        now.saturating_sub(self.at) < GRACE_MS && (seen == self.to || seen == self.from || Some(seen) == self.prior)
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveHold {
    /// Unix millis.
    pub since: u64,
    pub until: Option<u64>,
    pub rule: Option<usize>,
    /// Also ends when the active rule changes.
    pub boundary: bool,
}

impl ActiveHold {
    fn ended(&self, now: u64, rule: Option<usize>) -> bool {
        self.until.is_some_and(|u| now >= u) || (self.boundary && self.rule != rule)
    }
}

impl Hold {
    fn start(&self, now: u64, rule: Option<usize>) -> ActiveHold {
        let after = |hours: f64| now + (hours * 3_600_000.0) as u64;
        let (until, boundary) = match *self {
            Hold::Boundary { max_hours } => (max_hours.map(after), true),
            Hold::Fixed { hours } => (Some(after(hours)), false),
            Hold::Resume => (None, false),
        };
        ActiveHold { since: now, until, rule, boundary }
    }
}

/// A setpoint easing toward the active rule's, 1°F at a time.
#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveRamp {
    #[serde(skip)]
    rule: usize,
    pub from_f: i32,
    pub to_f: i32,
    /// Unix millis.
    pub started: u64,
    pub ends: u64,
}

impl ActiveRamp {
    /// Reaches `to_f` exactly at `ends`.
    fn at(&self, now: u64) -> i32 {
        let steps = self.to_f.abs_diff(self.from_f) as u64;
        let done = (now.saturating_sub(self.started) * steps / (self.ends - self.started)).min(steps);
        self.from_f + done as i32 * (self.to_f - self.from_f).signum()
    }
}

/// Decides when to command the unit. Idempotent: called repeatedly with the current state, it
/// re-asserts the target until the unit confirms it, and treats any change it can't attribute
/// to its own commands as manual.
#[derive(Default)]
pub struct Tracker {
    observed: Option<Managed>,
    sent: Option<Sent>,
    hold: Option<ActiveHold>,
    active: Option<usize>,
    ramp: Option<ActiveRamp>,
}

impl Tracker {
    pub fn active(&self) -> Option<usize> {
        self.active
    }

    pub fn hold(&self) -> Option<ActiveHold> {
        self.hold
    }

    pub fn ramp(&self) -> Option<ActiveRamp> {
        self.ramp
    }

    /// The target for right now, starting a ramp if the rule asks for one. A ramp only eases
    /// a unit already running in the rule's mode; otherwise it goes straight to the target.
    fn ramped(&mut self, now: u64, decision: &Decision, current: &Settings) -> Option<Action> {
        let target = decision.target?;
        let (Some(rule), Some(to_f), Some(minutes)) = (decision.rule, target.temp_f, target.ramp_minutes) else {
            self.ramp = None;
            return Some(target);
        };
        if self.ramp.is_some_and(|r| r.rule != rule || r.to_f != to_f) {
            self.ramp = None;
        }
        if self.ramp.is_none() && current.power && current.mode == target.mode && !target.matches(current) {
            let ends = now + minutes as u64 * 60_000;
            self.ramp = Some(ActiveRamp { rule, from_f: temp_f(current), to_f, started: now, ends });
        }
        // Kept until the unit gets there, so the last step still counts as part of the ramp.
        if target.matches(current) {
            self.ramp = None;
        }
        Some(Action { temp_f: self.ramp.map_or(target.temp_f, |r| Some(r.at(now))), ..target })
    }

    /// Ends a hold, so the schedule re-asserts on the next step.
    pub fn resume(&mut self) {
        self.hold = None;
        self.sent = None;
        self.ramp = None;
    }

    /// Returns the settings to send, if any. `now` is unix millis.
    pub fn step(&mut self, now: u64, schedule: &Schedule, decision: &Decision, current: &Settings) -> Option<Settings> {
        let seen = Managed::of(current);
        let changed = self.observed.replace(seen).is_some_and(|prev| prev != seen);
        self.active = decision.rule;
        if !schedule.enabled {
            self.resume();
            return None;
        }

        let target = if self.hold.is_some() { decision.target } else { self.ramped(now, decision, current) };
        let ours = target.is_some_and(|t| t.matches(current)) || self.sent.is_some_and(|s| s.explains(seen, now));
        if changed && !ours && target.is_some() {
            self.hold = Some(schedule.hold.start(now, decision.rule));
            self.ramp = None;
        }
        if self.hold.is_some_and(|h| h.ended(now, decision.rule)) {
            self.resume();
        }
        if self.hold.is_some() {
            return None;
        }

        let target = self.ramped(now, decision, current)?;
        if target.matches(current) {
            return None;
        }
        let want = target.apply(current);
        let to = Managed::of(&want);
        if self.sent.is_some_and(|s| s.to == to && now.saturating_sub(s.at) < RESEND_MS) {
            return None;
        }
        let prior = self.sent.filter(|s| now.saturating_sub(s.at) < GRACE_MS).map(|s| s.to);
        self.sent = Some(Sent { at: now, to, from: seen, prior });
        Some(want)
    }
}

fn starter(name: &str, when: When, power: bool, mode: u32, temp_f: Option<i32>, fan: Option<u32>) -> Rule {
    Rule { name: name.into(), enabled: true, when, then: Action { power, mode, temp_f, fan, ramp_minutes: None } }
}

fn window(from: &str, to: &str) -> When {
    When { from: Some(from.into()), to: Some(to.into()), ..When::default() }
}

/// The schedule written on first run. Off until the user enables it.
impl Default for Schedule {
    fn default() -> Self {
        let (heat, cool) = (1, 2);
        let rules = vec![
            starter("Pre-cool", When { high_min: Some(85.0), ..window("09:00", "12:00") }, true, cool, Some(73), None),
            starter("Hot day", When { outside_min: Some(80.0), ..When::default() }, true, cool, Some(76), None),
            starter(
                "Summer night",
                When { date_from: Some("05-01".into()), date_to: Some("09-30".into()), ..window("22:00", "07:00") },
                true,
                cool,
                Some(77),
                Some(6),
            ),
            starter("Cold night", When { outside_max: Some(45.0), ..window("22:00", "07:00") }, true, heat, Some(64), None),
            starter("Cold day", When { outside_max: Some(55.0), ..When::default() }, true, heat, Some(68), None),
            // Bounded, so that losing the weather leaves the unit alone rather than turning it off.
            starter("Mild", When { outside_min: Some(55.0), outside_max: Some(80.0), ..When::default() }, false, 0, None, None),
        ];
        Self { enabled: false, location: None, limits: Limits { min_f: 64, max_f: 80 }, hold: Hold::Boundary { max_hours: Some(4.0) }, quiet: Quiet::All, rules }
    }
}

#[cfg(test)]
mod tests;
