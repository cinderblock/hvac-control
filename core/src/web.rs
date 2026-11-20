//! The control UI and its JSON/SSE API, shared by the headless server and the desktop app.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{FromRef, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::ayla::{Device, Snapshot};
use crate::control::Settings;
use crate::schedule::Schedule;
use crate::scheduler::{Scheduler, Status};
use crate::weather::Location;

const INDEX: &str = include_str!("../../ui/index.html");
const APP_JS: &str = include_str!("../../ui/app.js");
const SCHEDULE_JS: &str = include_str!("../../ui/schedule.js");
const STYLE: &str = include_str!("../../ui/style.css");

fn asset(mime: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

fn ui() -> Router {
    Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", INDEX) }))
        .route("/app.js", get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }))
        .route("/schedule.js", get(|| async { asset("text/javascript; charset=utf-8", SCHEDULE_JS) }))
        .route("/style.css", get(|| async { asset("text/css; charset=utf-8", STYLE) }))
}

#[derive(Clone)]
struct App {
    dev: Arc<Device>,
    sched: Arc<Scheduler>,
}

impl FromRef<App> for Arc<Device> {
    fn from_ref(app: &App) -> Self {
        app.dev.clone()
    }
}

impl FromRef<App> for Arc<Scheduler> {
    fn from_ref(app: &App) -> Self {
        app.sched.clone()
    }
}

pub fn router(sched: Arc<Scheduler>) -> Router {
    let app = App { dev: sched.device().clone(), sched };
    let api = Router::new()
        .route("/state", get(state))
        .route("/settings", post(settings))
        .route("/refresh", post(refresh))
        .route("/events", get(events))
        .route("/schedule", get(get_schedule).post(save_schedule))
        .route("/schedule/status", get(schedule_status))
        .route("/schedule/resume", post(resume))
        .route("/geocode", get(geocode))
        .with_state(app);
    ui().nest("/api", api)
}

/// Serves the UI with every API call failing with `msg`, e.g. when there is no device config.
pub fn error_router(msg: String) -> Router {
    let fail = move || {
        let msg = msg.clone();
        async move { (StatusCode::SERVICE_UNAVAILABLE, msg) }
    };
    ui().route("/api/{*rest}", get(fail.clone()).post(fail))
}

pub async fn serve(listener: TcpListener, app: Router) -> Result<(), String> {
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

async fn state(State(dev): State<Arc<Device>>) -> Json<Snapshot> {
    Json(dev.snapshot())
}

async fn settings(State(dev): State<Arc<Device>>, Json(want): Json<Settings>) -> Result<Json<Snapshot>, (StatusCode, String)> {
    dev.apply_settings(&want).map_err(|e| (StatusCode::CONFLICT, e))?;
    Ok(Json(dev.snapshot()))
}

async fn refresh(State(dev): State<Arc<Device>>) -> StatusCode {
    dev.refresh();
    StatusCode::NO_CONTENT
}

async fn get_schedule(State(sched): State<Arc<Scheduler>>) -> Json<Schedule> {
    Json(sched.schedule())
}

async fn save_schedule(
    State(sched): State<Arc<Scheduler>>,
    Json(want): Json<Schedule>,
) -> Result<StatusCode, (StatusCode, String)> {
    sched.save(want).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn schedule_status(State(sched): State<Arc<Scheduler>>) -> Json<Status> {
    Json(sched.status())
}

async fn resume(State(sched): State<Arc<Scheduler>>) -> StatusCode {
    sched.resume();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct GeocodeQuery {
    q: String,
}

async fn geocode(
    State(sched): State<Arc<Scheduler>>,
    Query(query): Query<GeocodeQuery>,
) -> Result<Json<Vec<Location>>, (StatusCode, String)> {
    sched.geocode(&query.q).await.map(Json).map_err(|e| (StatusCode::BAD_GATEWAY, e))
}

/// The current snapshot and schedule status, then one of either per change.
async fn events(
    State(dev): State<Arc<Device>>,
    State(sched): State<Arc<Scheduler>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let updates = BroadcastStream::new(dev.subscribe()).filter_map(Result::ok);
    let unit = tokio_stream::once(dev.snapshot()).chain(updates).map(|snap| {
        Ok(Event::default().event("state").json_data(snap).expect("snapshot serializes"))
    });
    let updates = BroadcastStream::new(sched.subscribe()).filter_map(Result::ok);
    let schedule = tokio_stream::once(sched.status()).chain(updates).map(|status| {
        Ok(Event::default().event("schedule").json_data(status).expect("status serializes"))
    });
    Sse::new(unit.merge(schedule)).keep_alive(KeepAlive::default())
}
