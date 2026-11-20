//! Runs the schedule against the unit: keeps the weather current, evaluates the rules and
//! re-asserts the outcome. Rules live in `schedule.json` next to `devices.json`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, Timelike};
use serde::Serialize;
use tokio::sync::{broadcast, Notify};

use crate::ayla::{now_millis, Device};
use crate::schedule::{evaluate, next_change, validate, ActiveHold, ActiveRamp, Decision, Inputs, Quiet, Schedule, Tracker};
use crate::weather::{self, Forecast, Location};

const TICK: Duration = Duration::from_secs(30);
const WEATHER_REFRESH: Duration = Duration::from_secs(15 * 60);
const WEATHER_RETRY: Duration = Duration::from_secs(5 * 60);
/// Nothing to fetch until a location is set, which wakes the loop.
const NO_LOCATION_WAIT: Duration = Duration::from_secs(24 * 3600);
/// How far ahead to look for the rule change that ends an open-ended hold, in minutes.
const HOLD_HORIZON: u32 = 48 * 60;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WeatherStatus {
    pub outside_f: Option<f64>,
    pub high_f: Option<f64>,
    pub low_f: Option<f64>,
    /// Unix seconds of the data in use.
    pub fetched_at: Option<i64>,
    /// Why the last refresh failed; cleared by the next success.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub enabled: bool,
    pub has_location: bool,
    pub decision: Decision,
    pub hold: Option<ActiveHold>,
    /// Unix millis when the schedule takes back over; `None` if nothing will end the hold.
    /// Weather rules make this a forecast, not a promise.
    pub hold_ends: Option<u64>,
    pub ramp: Option<ActiveRamp>,
    pub weather: WeatherStatus,
    /// Whether the unit is at the target; `None` with no target or no reading.
    pub in_sync: Option<bool>,
}

struct State {
    schedule: Schedule,
    forecast: Option<Forecast>,
    weather_error: Option<String>,
    tracker: Tracker,
    status: Status,
}

pub struct Scheduler {
    dev: Arc<Device>,
    path: PathBuf,
    cache_path: PathBuf,
    http: reqwest::Client,
    state: Mutex<State>,
    /// Re-evaluate now.
    poke: Notify,
    /// Fetch the weather now.
    relocated: Notify,
    changes: broadcast::Sender<Status>,
}

fn read_schedule(path: &Path) -> Result<Option<Schedule>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let schedule = serde_json::from_str(&text).map_err(|e| format!("bad {}: {e}", path.display()))?;
    Ok(Some(schedule))
}

fn write_schedule(path: &Path, schedule: &Schedule) -> Result<(), String> {
    let text = serde_json::to_string_pretty(schedule).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

impl Scheduler {
    /// Loads the schedule from `dir`, writing the starter schedule on first run.
    pub fn new(dev: Arc<Device>, dir: &Path) -> Result<Arc<Self>, String> {
        let path = dir.join("schedule.json");
        let schedule = match read_schedule(&path)? {
            Some(schedule) => schedule,
            None => {
                let schedule = Schedule::default();
                write_schedule(&path, &schedule)?;
                schedule
            }
        };
        let cache_path = dir.join("weather.json");
        let forecast = weather::load_cache(&cache_path);
        Ok(Arc::new(Self {
            dev,
            path,
            cache_path,
            http: weather::client(),
            state: Mutex::new(State {
                schedule,
                forecast,
                weather_error: None,
                tracker: Tracker::default(),
                status: Status::default(),
            }),
            poke: Notify::new(),
            relocated: Notify::new(),
            changes: broadcast::channel(16).0,
        }))
    }

    pub fn device(&self) -> &Arc<Device> {
        &self.dev
    }

    pub fn schedule(&self) -> Schedule {
        self.state.lock().unwrap().schedule.clone()
    }

    pub fn status(&self) -> Status {
        self.state.lock().unwrap().status.clone()
    }

    /// Receives the status after every change.
    pub fn subscribe(&self) -> broadcast::Receiver<Status> {
        self.changes.subscribe()
    }

    pub fn save(&self, schedule: Schedule) -> Result<(), String> {
        validate(&schedule)?;
        write_schedule(&self.path, &schedule)?;
        let relocated = {
            let mut st = self.state.lock().unwrap();
            let relocated = st.schedule.location != schedule.location;
            st.schedule = schedule;
            relocated
        };
        if relocated {
            self.relocated.notify_one();
        }
        self.poke.notify_one();
        Ok(())
    }

    /// Ends a manual hold.
    pub fn resume(&self) {
        self.state.lock().unwrap().tracker.resume();
        self.poke.notify_one();
    }

    pub async fn geocode(&self, name: &str) -> Result<Vec<Location>, String> {
        weather::geocode(&self.http, name).await
    }

    fn inputs(st: &State, now: DateTime<Local>) -> Inputs {
        let forecast = st
            .forecast
            .as_ref()
            .filter(|f| st.schedule.location.as_ref().is_some_and(|l| f.is_for(l)));
        let range = forecast.and_then(|f| f.high_low(now.date_naive(), &Local));
        Inputs {
            now: now.naive_local(),
            outside_f: forecast.and_then(|f| f.outside_at(now.timestamp())),
            high_f: range.map(|r| r.0),
            low_f: range.map(|r| r.1),
        }
    }

    fn hold_ends(st: &State, hold: &ActiveHold, rule: Option<usize>, now: DateTime<Local>) -> Option<u64> {
        if !hold.boundary {
            return hold.until;
        }
        let start = now.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap_or(now);
        let horizon = hold.until.map_or(HOLD_HORIZON, |u| u.saturating_sub(now_millis()).div_ceil(60_000) as u32);
        let ahead = |m: u32| Self::inputs(st, start + chrono::Duration::minutes(m as i64));
        let change = next_change(&st.schedule, rule, horizon.min(HOLD_HORIZON), ahead);
        change.map(|m| start.timestamp_millis() as u64 + m as u64 * 60_000).or(hold.until)
    }

    fn tick(&self) {
        let snap = self.dev.snapshot();
        let (status, want, quiet) = {
            let mut st = self.state.lock().unwrap();
            let st = &mut *st;
            let now = Local::now();
            let inputs = Self::inputs(st, now);
            let decision = evaluate(&st.schedule, &inputs, st.tracker.active());
            // Nothing is sent to an unreachable unit, or the queue would replay stale commands.
            let reading = snap.settings.filter(|_| snap.online);
            let want = reading.and_then(|s| st.tracker.step(now_millis(), &st.schedule, &decision, &s));
            let ramp = st.tracker.ramp();
            let in_sync = decision.target.zip(reading).map(|(t, s)| ramp.is_some() || t.matches(&s));
            let hold = st.tracker.hold();
            let quiet = match st.schedule.quiet {
                Quiet::Off => false,
                Quiet::Ramps => ramp.is_some(),
                Quiet::All => true,
            };
            let status = Status {
                enabled: st.schedule.enabled,
                has_location: st.schedule.location.is_some(),
                hold,
                hold_ends: hold.and_then(|h| Self::hold_ends(st, &h, decision.rule, now)),
                ramp,
                weather: WeatherStatus {
                    outside_f: inputs.outside_f,
                    high_f: inputs.high_f,
                    low_f: inputs.low_f,
                    fetched_at: st.forecast.as_ref().map(|f| f.fetched_at),
                    error: st.weather_error.clone(),
                },
                decision,
                in_sync,
            };
            let changed = st.status != status;
            st.status = status.clone();
            (changed.then_some(status), want, quiet)
        };
        if let Some(status) = status {
            let _ = self.changes.send(status);
        }
        if let Some(want) = want {
            eprintln!("[schedule] applying {want:?} (quiet: {quiet})");
            let applied = if quiet { self.dev.apply_settings_quietly(&want) } else { self.dev.apply_settings(&want) };
            if let Err(e) = applied {
                eprintln!("[schedule] {e}");
            }
        }
    }

    async fn refresh_weather(&self, location: &Location) -> Duration {
        let now = chrono::Utc::now().timestamp();
        let fetched = weather::fetch(&self.http, location, now).await;
        let mut st = self.state.lock().unwrap();
        match fetched {
            Ok(forecast) => {
                st.weather_error = None;
                if let Err(e) = weather::save_cache(&self.cache_path, &forecast) {
                    eprintln!("[weather] {e}");
                }
                st.forecast = Some(forecast);
                WEATHER_REFRESH
            }
            Err(e) => {
                eprintln!("[weather] {e}");
                st.weather_error = Some(e);
                WEATHER_RETRY
            }
        }
    }

    async fn weather_loop(self: Arc<Self>) {
        loop {
            let location = self.state.lock().unwrap().schedule.location.clone();
            let wait = match location {
                Some(location) => self.refresh_weather(&location).await,
                None => NO_LOCATION_WAIT,
            };
            self.poke.notify_one();
            let _ = tokio::time::timeout(wait, self.relocated.notified()).await;
        }
    }

    /// Runs forever.
    pub async fn run(self: Arc<Self>) {
        tokio::spawn(self.clone().weather_loop());
        let mut tick = tokio::time::interval(TICK);
        let mut unit = self.dev.subscribe();
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = unit.recv() => {}
                _ = self.poke.notified() => {}
            }
            self.tick();
        }
    }
}
