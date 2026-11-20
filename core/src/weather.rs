//! Outside temperature and hourly forecast from Open-Meteo, with an on-disk cache.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};

const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
const GEOCODE_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
/// `current_f` is trusted this long; after that the hourly forecast stands in for it.
const LIVE_SECS: i64 = 3600;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Forecast {
    /// Where this was fetched for, so a cache from an old location is never used.
    pub latitude: f64,
    pub longitude: f64,
    /// Unix seconds.
    pub fetched_at: i64,
    pub current_f: f64,
    /// (unix seconds, degF), ascending.
    pub hours: Vec<(i64, f64)>,
}

impl Forecast {
    pub fn is_for(&self, l: &Location) -> bool {
        self.latitude == l.latitude && self.longitude == l.longitude
    }

    /// The live reading while fresh, then the forecast. `None` once the forecast has run out.
    pub fn outside_at(&self, now: i64) -> Option<f64> {
        if now - self.fetched_at < LIVE_SECS {
            return Some(self.current_f);
        }
        let after = self.hours.iter().position(|h| h.0 > now)?;
        let (t1, v1) = self.hours[after];
        let (t0, v0) = self.hours[after.checked_sub(1)?];
        Some(v0 + (v1 - v0) * (now - t0) as f64 / (t1 - t0) as f64)
    }

    /// The high and low forecast for a calendar day in `tz`.
    pub fn high_low<Tz: TimeZone>(&self, day: NaiveDate, tz: &Tz) -> Option<(f64, f64)> {
        let local = |t: i64| DateTime::from_timestamp(t, 0).map(|d| d.with_timezone(tz).date_naive());
        let temps = self.hours.iter().filter(|h| local(h.0) == Some(day)).map(|h| h.1);
        temps.fold(None, |range, t| match range {
            None => Some((t, t)),
            Some((high, low)) => Some((t.max(high), t.min(low))),
        })
    }
}

#[derive(Deserialize)]
struct Response {
    current: Current,
    hourly: Hourly,
}

#[derive(Deserialize)]
struct Current {
    temperature_2m: f64,
}

#[derive(Deserialize)]
struct Hourly {
    time: Vec<i64>,
    temperature_2m: Vec<Option<f64>>,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("http client")
}

fn parse(body: &str, l: &Location, now: i64) -> Result<Forecast, String> {
    let r: Response = serde_json::from_str(body).map_err(|e| format!("bad forecast: {e}"))?;
    let temps = r.hourly.temperature_2m;
    let hours = r.hourly.time.into_iter().zip(temps).filter_map(|(t, v)| Some((t, v?))).collect();
    Ok(Forecast { latitude: l.latitude, longitude: l.longitude, fetched_at: now, current_f: r.current.temperature_2m, hours })
}

pub async fn fetch(http: &reqwest::Client, l: &Location, now: i64) -> Result<Forecast, String> {
    let query = [
        ("latitude", l.latitude.to_string()),
        ("longitude", l.longitude.to_string()),
        ("current", "temperature_2m".to_string()),
        ("hourly", "temperature_2m".to_string()),
        ("temperature_unit", "fahrenheit".to_string()),
        ("timeformat", "unixtime".to_string()),
        ("timezone", "auto".to_string()),
        ("forecast_days", "3".to_string()),
    ];
    let reply = http.get(FORECAST_URL).query(&query).send().await.map_err(|e| e.to_string())?;
    let reply = reply.error_for_status().map_err(|e| e.to_string())?;
    parse(&reply.text().await.map_err(|e| e.to_string())?, l, now)
}

#[derive(Deserialize)]
struct Places {
    #[serde(default)]
    results: Vec<Place>,
}

#[derive(Deserialize)]
struct Place {
    name: String,
    admin1: Option<String>,
    country: Option<String>,
    latitude: f64,
    longitude: f64,
}

fn parse_places(body: &str) -> Result<Vec<Location>, String> {
    let places: Places = serde_json::from_str(body).map_err(|e| format!("bad geocoding reply: {e}"))?;
    Ok(places
        .results
        .into_iter()
        .map(|p| {
            let parts: Vec<String> = [Some(p.name), p.admin1, p.country].into_iter().flatten().collect();
            Location { name: parts.join(", "), latitude: p.latitude, longitude: p.longitude }
        })
        .collect())
}

pub async fn geocode(http: &reqwest::Client, name: &str) -> Result<Vec<Location>, String> {
    let query = [("name", name), ("count", "6"), ("format", "json")];
    let reply = http.get(GEOCODE_URL).query(&query).send().await.map_err(|e| e.to_string())?;
    let reply = reply.error_for_status().map_err(|e| e.to_string())?;
    parse_places(&reply.text().await.map_err(|e| e.to_string())?)
}

pub fn load_cache(path: &Path) -> Option<Forecast> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn save_cache(path: &Path, f: &Forecast) -> Result<(), String> {
    let text = serde_json::to_string(f).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const HOUR: i64 = 3600;
    // 2026-07-15 00:00 at UTC-7.
    const MIDNIGHT: i64 = 1_784_098_800;

    fn place() -> Location {
        Location { name: "x".into(), latitude: 1.5, longitude: -2.5 }
    }

    fn forecast(hours: Vec<(i64, f64)>, fetched_at: i64) -> Forecast {
        Forecast { latitude: 1.5, longitude: -2.5, fetched_at, current_f: 70.0, hours }
    }

    #[test]
    fn parses_an_open_meteo_reply_skipping_gaps() {
        let body = r#"{"latitude":1.5,"utc_offset_seconds":-25200,"timezone":"America/Los_Angeles",
            "current":{"time":100,"interval":900,"temperature_2m":71.3},
            "hourly":{"time":[0,3600,7200],"temperature_2m":[60.5,null,62.5]}}"#;
        let f = parse(body, &place(), 100).unwrap();
        assert_eq!(f.current_f, 71.3);
        assert_eq!(f.hours, [(0, 60.5), (7200, 62.5)]);
        assert!(f.is_for(&place()));
    }

    #[test]
    fn rejects_an_error_reply() {
        assert!(parse(r#"{"error":true,"reason":"nope"}"#, &place(), 0).is_err());
    }

    #[test]
    fn outside_uses_the_live_reading_while_fresh() {
        let f = forecast(vec![(0, 50.0), (HOUR, 60.0)], 0);
        assert_eq!(f.outside_at(HOUR - 1), Some(70.0));
    }

    #[test]
    fn outside_falls_back_to_the_forecast_during_an_outage() {
        let f = forecast(vec![(0, 50.0), (2 * HOUR, 60.0), (4 * HOUR, 80.0)], -10 * HOUR);
        assert_eq!(f.outside_at(HOUR), Some(55.0));
        assert_eq!(f.outside_at(2 * HOUR), Some(60.0));
    }

    #[test]
    fn outside_is_unknown_once_the_forecast_runs_out() {
        let f = forecast(vec![(0, 50.0), (HOUR, 60.0)], -10 * HOUR);
        assert_eq!(f.outside_at(HOUR), None);
        assert_eq!(f.outside_at(-5 * HOUR), None);
    }

    #[test]
    fn high_low_covers_only_the_local_day() {
        let tz = FixedOffset::west_opt(7 * 3600).unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();
        let hours = vec![(MIDNIGHT - HOUR, 99.0), (MIDNIGHT, 60.0), (MIDNIGHT + 14 * HOUR, 88.0), (MIDNIGHT + 24 * HOUR, 40.0)];
        let f = forecast(hours, 0);
        assert_eq!(f.high_low(day, &tz), Some((88.0, 60.0)));
        assert_eq!(f.high_low(day.succ_opt().unwrap().succ_opt().unwrap(), &tz), None);
    }

    #[test]
    fn cache_is_not_used_for_another_location() {
        let elsewhere = Location { latitude: 9.0, ..place() };
        assert!(!forecast(vec![], 0).is_for(&elsewhere));
    }

    #[test]
    fn cache_roundtrips() {
        let path = std::env::temp_dir().join(format!("hvac-weather-test-{}.json", std::process::id()));
        let f = forecast(vec![(0, 50.0)], 7);
        save_cache(&path, &f).unwrap();
        let back = load_cache(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(back, Some(f));
    }

    #[test]
    fn places_join_their_regions_and_tolerate_no_results() {
        let body = r#"{"results":[{"name":"Springfield","admin1":"Oregon","country":"United States","latitude":44.0,"longitude":-123.0}]}"#;
        let places = parse_places(body).unwrap();
        assert_eq!(places[0].name, "Springfield, Oregon, United States");
        assert_eq!(parse_places(r#"{"generationtime_ms":0.1}"#).unwrap(), []);
    }
}
