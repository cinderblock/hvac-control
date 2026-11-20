//! Ayla Networks LAN-mode client.
//!
//! Flow: we POST `local_reg.json` to the unit, it calls back into our HTTP
//! server for a key exchange, then polls `commands.json` whenever we PUT
//! `local_reg.json` with `notify: 1`, and POSTs encrypted property updates.

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::distr::{Alphanumeric, SampleString};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::{broadcast, Notify};

use crate::control;

pub const LISTEN_PORT: u16 = 10275;
/// How many ports from `LISTEN_PORT` up we listen on; see `MAX_STALLS`.
const LISTEN_PORTS: u16 = 4;
const KEEP_ALIVE: Duration = Duration::from_secs(10);
/// If the unit hasn't fetched commands this long after a notify, start a fresh session.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(6);
/// The module can stop fetching commands while still accepting key exchanges and notifies.
/// Re-keying doesn't help, and the cause and cure are unknown. After this many ignored notifies
/// we try the next port, and back off once they have all been tried.
const MAX_STALLS: u32 = 5;
const STALL_QUIET: Duration = Duration::from_secs(150);
const MAX_FAILURES: u32 = 3;

type HmacSha256 = Hmac<Sha256>;
type Enc = cbc::Encryptor<aes::Aes256>;
type Dec = cbc::Decryptor<aes::Aes256>;

#[derive(Clone, Debug, Deserialize)]
pub struct PropertyInfo {
    pub name: String,
    pub base_type: String,
}

/// One entry of `devices.json`, as written by `research/discover.py`.
#[derive(Clone, Debug, Deserialize)]
pub struct DeviceConfig {
    pub product_name: String,
    pub lan_ip: String,
    pub lanip_key: String,
    pub lanip_key_id: u64,
    #[serde(default)]
    pub properties: Vec<PropertyInfo>,
}

fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac takes any key size");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn derive(lan_key: &[u8], seed: &[u8], suffix: u8) -> [u8; 32] {
    let msg = [seed, &[suffix]].concat();
    hmac(lan_key, &[&hmac(lan_key, &msg)[..], &msg].concat())
}

/// One direction of an established session. The CBC chain continues across messages.
struct Sender {
    sign_key: [u8; 32],
    cipher: Enc,
}

struct Receiver {
    sign_key: [u8; 32],
    cipher: Dec,
}

impl Sender {
    fn new(lan_key: &[u8], seed: &[u8]) -> Self {
        let iv = derive(lan_key, seed, b'2');
        Self {
            sign_key: derive(lan_key, seed, b'0'),
            cipher: Enc::new(&derive(lan_key, seed, b'1').into(), iv[..16].into()),
        }
    }

    fn seal(&mut self, plain: &[u8]) -> Value {
        let mut buf = plain.to_vec();
        buf.resize(plain.len().div_ceil(16) * 16, 0);
        for block in buf.chunks_exact_mut(16) {
            self.cipher.encrypt_block_mut(block.into());
        }
        json!({ "enc": B64.encode(&buf), "sign": B64.encode(hmac(&self.sign_key, plain)) })
    }
}

impl Receiver {
    fn new(lan_key: &[u8], seed: &[u8]) -> Self {
        let iv = derive(lan_key, seed, b'2');
        Self {
            sign_key: derive(lan_key, seed, b'0'),
            cipher: Dec::new(&derive(lan_key, seed, b'1').into(), iv[..16].into()),
        }
    }

    /// Decrypts and verifies; returns the plaintext.
    fn open(&mut self, enc: &str, sign: &str) -> Result<Vec<u8>, String> {
        let mut buf = B64.decode(enc).map_err(|e| e.to_string())?;
        if buf.len() % 16 != 0 {
            return Err("ciphertext not block aligned".into());
        }
        for block in buf.chunks_exact_mut(16) {
            self.cipher.decrypt_block_mut(block.into());
        }
        let end = buf.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        let plain = &buf[..end];
        if B64.encode(hmac(&self.sign_key, plain)) != sign {
            return Err("bad signature".into());
        }
        Ok(plain.to_vec())
    }
}

struct Session {
    tx: Sender,
    rx: Receiver,
    seq: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub name: String,
    pub ip: String,
    pub online: bool,
    /// Unix millis of the last message from the unit.
    pub last_seen: Option<u64>,
    pub pending: usize,
    pub props: BTreeMap<String, Value>,
    /// Where the unit is headed, including writes it hasn't confirmed.
    pub settings: Option<control::Settings>,
    /// What the unit last reported.
    pub reported: Option<control::Settings>,
}

/// A `t_control_value` we've sent but the unit hasn't reported back.
struct Unconfirmed {
    value: u32,
    /// When the unit fetched the write.
    fetched_at: Option<Instant>,
    /// Reads with this `cmd_id` or higher were issued after that fetch.
    after: u64,
    /// How many of those reads disagreed.
    mismatches: u32,
}

/// What a `t_control_value` report means for the unconfirmed write.
#[derive(Debug, PartialEq)]
enum Report {
    /// May predate the write; says nothing about it.
    Stale,
    /// Disagrees, but the change could still be reaching the AC's board.
    Recheck,
    /// The write landed, or definitely didn't.
    Settled,
}

struct Command {
    data: Value,
    /// Property to update optimistically once the unit has fetched the command.
    applies: Option<(String, Value)>,
    /// Properties this command reads, retried if no reply arrives.
    asks: Vec<String>,
    /// The raw `t_control_value` this command writes.
    control: Option<u32>,
}

#[derive(Default)]
struct Inner {
    session: Option<Session>,
    /// Writes go ahead of reads.
    writes: VecDeque<Command>,
    reads: VecDeque<Command>,
    props: BTreeMap<String, Value>,
    online: bool,
    last_seen: Option<u64>,
    notified_at: Option<Instant>,
    /// Notifies in a row the unit ignored.
    stalls: u32,
    /// Which of `LISTEN_PORTS` we register with the unit.
    port_offset: u16,
    /// Reads the unit has fetched but not answered yet, and when.
    awaiting: BTreeMap<String, Instant>,
    unconfirmed: Option<Unconfirmed>,
    /// `t_beep` as first reported, which is what a mute restores.
    beep_normal: Option<i64>,
    muted: bool,
    next_cmd_id: u64,
}

impl Inner {
    fn pending(&self) -> usize {
        self.writes.len() + self.reads.len()
    }

    /// Forgets an unconfirmed write the unit evidently didn't take; true if it did so.
    fn expire_unconfirmed(&mut self) -> bool {
        let stale = self
            .unconfirmed
            .as_ref()
            .and_then(|u| u.fetched_at)
            .is_some_and(|t| t.elapsed() > CONFIRM_TIMEOUT);
        if stale {
            self.unconfirmed = None;
        }
        stale
    }

    /// The control value to show and build on: our latest write, else what the unit reported.
    fn control(&self) -> Option<u32> {
        let reported = || self.props.get("t_control_value").and_then(Value::as_i64).map(|c| c as u32);
        self.unconfirmed.as_ref().map(|u| u.value).or_else(reported)
    }

    /// Takes a report from the unit. Only a read issued after the unit took the write can
    /// show it failed; `expire_unconfirmed` covers one whose reads never come back.
    fn report_control(&mut self, value: &Value, cmd_id: Option<u64>) -> Report {
        let reported = value.as_i64().map(|c| control::values_only(c as u32));
        let Some(u) = self.unconfirmed.as_mut() else { return Report::Stale };
        if Some(u.value) == reported {
            self.unconfirmed = None;
            return Report::Settled;
        }
        if u.fetched_at.is_none() || cmd_id.is_none_or(|id| id < u.after) {
            return Report::Stale;
        }
        u.mismatches += 1;
        if u.mismatches < MAX_MISMATCHES {
            return Report::Recheck;
        }
        self.unconfirmed = None;
        Report::Settled
    }
}

pub struct Device {
    cfg: DeviceConfig,
    ip: IpAddr,
    inner: Mutex<Inner>,
    wake: Notify,
    changes: broadcast::Sender<Snapshot>,
}

/// Properties polled periodically.
const STATUS_PROPS: &[&str] = &["t_control_value", "t_ftkt_start", "f_temp_in", "t_power", "t_work_mode", "t_beep"];
const STATUS_INTERVAL: Duration = Duration::from_secs(30);
/// Replies get lost on a weak link; re-ask after this long.
const READ_TIMEOUT: Duration = Duration::from_secs(8);
/// How long after the unit fetches a write we wait to see it reported back.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
/// Later reads that must disagree before a write counts as failed.
const MAX_MISMATCHES: u32 = 3;
/// The firmware answers only the first read of a multi-command batch.
const READS_PER_COMMAND: usize = 1;

pub(crate) fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Device {
    pub fn new(cfg: DeviceConfig) -> Result<Arc<Self>, String> {
        let ip = cfg.lan_ip.parse().map_err(|e| format!("bad lan_ip {:?}: {e}", cfg.lan_ip))?;
        Ok(Arc::new(Self {
            cfg,
            ip,
            inner: Mutex::default(),
            wake: Notify::new(),
            changes: broadcast::channel(64).0,
        }))
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner.lock().unwrap();
        let settings = inner.control().map(control::decode);
        let reported = inner.props.get("t_control_value").and_then(Value::as_i64).map(|c| control::decode(c as u32));
        Snapshot {
            name: self.cfg.product_name.clone(),
            ip: self.cfg.lan_ip.clone(),
            online: inner.online,
            last_seen: inner.last_seen,
            pending: inner.pending(),
            props: inner.props.clone(),
            settings,
            reported,
        }
    }

    /// Receives a snapshot after every state change.
    pub fn subscribe(&self) -> broadcast::Receiver<Snapshot> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        let _ = self.changes.send(self.snapshot());
    }

    fn base_type(&self, name: &str) -> &str {
        self.cfg
            .properties
            .iter()
            .find(|p| p.name == name)
            .map_or("integer", |p| p.base_type.as_str())
    }

    pub fn queue_read(&self, names: &[&str]) {
        {
            let mut inner = self.inner.lock().unwrap();
            for chunk in names.chunks(READS_PER_COMMAND) {
                let cmds: Vec<Value> = chunk
                    .iter()
                    .map(|name| {
                        let cmd_id = inner.next_cmd_id;
                        inner.next_cmd_id += 1;
                        json!({ "cmd": {
                            "method": "GET",
                            "resource": format!("property.json?name={name}"),
                            "uri": "/local_lan/property/datapoint.json",
                            "data": "",
                            "cmd_id": cmd_id,
                        }})
                    })
                    .collect();
                let asks = chunk.iter().map(|n| n.to_string()).collect();
                inner.reads.push_back(Command { data: json!({ "cmds": cmds }), applies: None, asks, control: None });
            }
        }
        self.wake.notify_one();
        self.changed();
    }

    fn write_data(&self, name: &str, value: &Value) -> Value {
        let id = Alphanumeric.sample_string(&mut rand::rng(), 8);
        json!({ "properties": [{ "property": {
            "base_type": self.base_type(name),
            "name": name,
            "value": value,
            "id": id,
        }}]})
    }

    pub fn queue_write(&self, name: &str, value: Value) {
        let data = self.write_data(name, &value);
        self.inner.lock().unwrap().writes.push_back(Command {
            data,
            applies: Some((name.to_string(), value)),
            asks: Vec::new(),
            control: None,
        });
        self.wake.notify_one();
        self.changed();
    }

    /// Queues a control write, folding it into one the unit hasn't fetched yet so that
    /// quick changes reach it (and beep) only once.
    fn queue_control(&self, raw: u32) {
        {
            let mut inner = self.inner.lock().unwrap();
            // The unit reports the value without change flags; mirror that.
            let value = control::values_only(raw);
            inner.unconfirmed = Some(Unconfirmed { value, fetched_at: None, after: 0, mismatches: 0 });
            let queued = inner.writes.iter_mut().rev().find(|c| c.control.is_some());
            let merged = queued.as_ref().map(|c| control::merge_flags(c.control.unwrap_or(0), raw));
            match (queued, merged) {
                (Some(queued), Some(merged)) if !control::mixes_power(merged) => {
                    queued.data = self.write_data("t_control_value", &json!(merged));
                    queued.control = Some(merged);
                }
                _ => {
                    let data = self.write_data("t_control_value", &json!(raw));
                    inner.writes.push_back(Command { data, applies: None, asks: Vec::new(), control: Some(raw) });
                }
            }
        }
        self.wake.notify_one();
        self.changed();
    }

    fn apply(&self, want: &control::Settings, quiet: bool) -> Result<(), String> {
        let current = self.inner.lock().unwrap().control().ok_or("current state not known yet")?;
        let next = control::encode_changes(current, want);
        if control::values_only(next) == control::values_only(current) {
            return Ok(());
        }
        if quiet {
            self.mute();
        }
        // Sent unsigned, as AirCon does; the unit reads it back signed.
        match control::split_power(current, next) {
            Some((settings, power)) => {
                self.queue_control(settings);
                self.queue_control(power);
            }
            None => self.queue_control(next),
        }
        Ok(())
    }

    /// Queues a `t_control_value` write that moves the unit to `want`.
    pub fn apply_settings(&self, want: &control::Settings) -> Result<(), String> {
        self.apply(want, false)
    }

    /// Like `apply_settings`, with the unit's beep off until the change has landed.
    pub fn apply_settings_quietly(&self, want: &control::Settings) -> Result<(), String> {
        self.apply(want, true)
    }

    fn mute(&self) {
        let mute = {
            let mut inner = self.inner.lock().unwrap();
            let mute = !inner.muted && inner.beep_normal.is_some_and(|b| b != 0);
            inner.muted |= mute;
            mute
        };
        if mute {
            self.queue_write("t_beep", json!(0));
        }
    }

    fn unmute(&self) {
        let restore = {
            let mut inner = self.inner.lock().unwrap();
            let restore = inner.beep_normal.filter(|_| inner.muted);
            inner.muted = false;
            restore
        };
        if let Some(beep) = restore {
            self.queue_write("t_beep", json!(beep));
        }
    }

    /// Re-sends a restore that got lost, so the unit doesn't stay silent.
    fn check_beep(&self) {
        let lost = {
            let inner = self.inner.lock().unwrap();
            let reported = inner.props.get("t_beep").and_then(Value::as_i64);
            inner.beep_normal.filter(|&b| !inner.muted && inner.writes.is_empty() && reported.is_some_and(|r| r != b))
        };
        if let Some(beep) = lost {
            eprintln!("[ayla] t_beep is off normal; restoring");
            self.queue_write("t_beep", json!(beep));
        }
    }

    /// Sends a request to the Wi-Fi module itself (not the AC); any reply arrives at `module_reply`.
    pub fn queue_module(&self, method: &str, resource: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            let cmd_id = inner.next_cmd_id;
            inner.next_cmd_id += 1;
            let data = json!({ "cmds": [{ "cmd": {
                "method": method,
                "resource": resource,
                "uri": format!("/local_lan/{resource}"),
                "data": "",
                "cmd_id": cmd_id,
            }}]});
            inner.reads.push_back(Command { data, applies: None, asks: Vec::new(), control: None });
        }
        self.wake.notify_one();
        self.changed();
    }

    pub fn refresh(&self) {
        self.queue_read(STATUS_PROPS);
    }

    /// Runs the callback server, keep-alive loop and status poller forever.
    pub async fn run(self: Arc<Self>) -> Result<(), String> {
        let app = Router::new()
            .route("/local_lan/key_exchange.json", post(key_exchange))
            .route("/local_lan/commands.json", get(commands))
            .route("/local_lan/property/datapoint.json", post(datapoint))
            .route("/local_lan/property/datapoint/ack.json", post(datapoint))
            .route("/local_lan/node/property/datapoint.json", post(datapoint))
            .route("/local_lan/node/property/datapoint/ack.json", post(datapoint))
            .route("/local_lan/{*resource}", post(module_reply))
            .with_state(self.clone());
        for port in LISTEN_PORT..LISTEN_PORT + LISTEN_PORTS {
            let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
                .await
                .map_err(|e| format!("cannot listen on port {port}: {e}"))?;
            let app = app.clone();
            tokio::spawn(async move {
                axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
            });
        }

        // Outputs (`f_*`) push on change, so only the polled inputs are read up front;
        // a long startup queue on a lossy link delays the state the UI needs.
        self.refresh();
        let poller = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(READ_TIMEOUT / 2);
            let mut last_refresh = Instant::now();
            loop {
                tick.tick().await;
                let (lost, idle, expired) = {
                    let mut inner = poller.inner.lock().unwrap();
                    let expired = inner.expire_unconfirmed();
                    let queued: Vec<String> = inner.reads.iter().flat_map(|c| c.asks.clone()).collect();
                    let mut lost = Vec::new();
                    inner.awaiting.retain(|name, at| {
                        let stale = at.elapsed() > READ_TIMEOUT;
                        if stale && !queued.contains(name) {
                            lost.push(name.clone());
                        }
                        !stale
                    });
                    (lost, inner.reads.is_empty(), expired)
                };
                if expired {
                    eprintln!("[ayla] write never confirmed; showing the reported state");
                    poller.unmute();
                    poller.changed();
                }
                if !lost.is_empty() {
                    eprintln!("[ayla] no reply for {lost:?}; asking again");
                    poller.queue_read(&lost.iter().map(String::as_str).collect::<Vec<_>>());
                } else if idle && last_refresh.elapsed() >= STATUS_INTERVAL {
                    last_refresh = Instant::now();
                    poller.check_beep();
                    poller.refresh();
                }
            }
        });

        self.keep_alive().await;
        Ok(())
    }

    async fn keep_alive(&self) {
        // The unit's HTTP parser wants `Content-Length` capitalised and closes every
        // connection after one request.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .http1_title_case_headers()
            .pool_max_idle_per_host(0)
            .build()
            .expect("http client");
        let url = format!("http://{}/local_reg.json", self.ip);
        let mut failures = 0;
        loop {
            let quiet = {
                let mut inner = self.inner.lock().unwrap();
                // A stalled notify usually means the unit dropped our session.
                if inner.notified_at.is_some_and(|t| t.elapsed() > NOTIFY_TIMEOUT) {
                    inner.session = None;
                    inner.notified_at = None;
                    inner.stalls += 1;
                }
                inner.stalls >= MAX_STALLS
            };
            if quiet {
                let offset = {
                    let mut inner = self.inner.lock().unwrap();
                    inner.stalls = 0;
                    inner.port_offset = (inner.port_offset + 1) % LISTEN_PORTS;
                    inner.port_offset
                };
                eprintln!("[ayla] unit ignored {MAX_STALLS} notifies; moving to port {}", LISTEN_PORT + offset);
                if offset == 0 {
                    eprintln!("[ayla] every port ignored; leaving it alone for {STALL_QUIET:?}");
                    tokio::time::sleep(STALL_QUIET).await;
                }
            }
            let (method, notify, port) = {
                let mut inner = self.inner.lock().unwrap();
                let pending = inner.pending() > 0;
                let method = if inner.session.is_some() {
                    reqwest::Method::PUT
                } else {
                    reqwest::Method::POST
                };
                if pending && inner.notified_at.is_none() {
                    inner.notified_at = Some(Instant::now());
                }
                (method, pending, LISTEN_PORT + inner.port_offset)
            };
            let body = json!({ "local_reg": {
                "ip": local_ip_for(self.ip).map(|ip| ip.to_string()).unwrap_or_default(),
                "notify": notify as u8,
                "port": port,
                "uri": "/local_lan",
            }});
            let result = http.request(method.clone(), &url).json(&body).send().await;
            let ok = matches!(&result, Ok(r) if r.status() == StatusCode::ACCEPTED);
            if !ok {
                eprintln!("[ayla] local_reg {method} failed: {:?}", result.map(|r| r.status()));
            }
            if ok {
                failures = 0;
            } else {
                failures += 1;
                if failures >= MAX_FAILURES {
                    let was_online = {
                        let mut inner = self.inner.lock().unwrap();
                        inner.session = None;
                        std::mem::replace(&mut inner.online, false)
                    };
                    if was_online {
                        self.changed();
                    }
                }
            }
            let wait = if notify || !ok { Duration::from_secs(2) } else { KEEP_ALIVE };
            let _ = tokio::time::timeout(wait, self.wake.notified()).await;
        }
    }

    fn mark_seen(&self, inner: &mut Inner) {
        inner.online = true;
        inner.last_seen = Some(now_millis());
    }
}

/// The local address the OS would use to reach `peer`.
fn local_ip_for(peer: IpAddr) -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect((peer, 80)).ok()?;
    Some(sock.local_addr().ok()?.ip())
}

type Shared = State<Arc<Device>>;
type Peer = ConnectInfo<SocketAddr>;

#[derive(Deserialize)]
struct KeyExchangeBody {
    key_exchange: KeyExchange,
}

#[derive(Deserialize)]
struct KeyExchange {
    random_1: String,
    time_1: u64,
    key_id: u64,
}

async fn key_exchange(
    State(dev): Shared,
    ConnectInfo(peer): Peer,
    Json(body): Json<KeyExchangeBody>,
) -> Result<Json<Value>, StatusCode> {
    if peer.ip() != dev.ip {
        return Err(StatusCode::FORBIDDEN);
    }
    let k = body.key_exchange;
    if k.key_id != dev.cfg.lanip_key_id {
        eprintln!("unit wants key_id {}, have {}; rerun discovery", k.key_id, dev.cfg.lanip_key_id);
        return Err(StatusCode::NOT_FOUND);
    }
    let random_2 = Alphanumeric.sample_string(&mut rand::rng(), 16);
    let time_2 = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() % (1 << 40)) as u64;
    let (r1, t1, r2, t2) = (k.random_1.as_bytes(), k.time_1.to_string(), random_2.as_bytes(), time_2.to_string());
    let key = dev.cfg.lanip_key.as_bytes();
    let session = Session {
        tx: Sender::new(key, &[r1, r2, t1.as_bytes(), t2.as_bytes()].concat()),
        rx: Receiver::new(key, &[r2, r1, t2.as_bytes(), t1.as_bytes()].concat()),
        seq: 0,
    };
    {
        let mut inner = dev.inner.lock().unwrap();
        inner.session = Some(session);
        dev.mark_seen(&mut inner);
        eprintln!("[ayla] key exchange (key_id {})", k.key_id);
    }
    dev.changed();
    Ok(Json(json!({ "random_2": random_2, "time_2": time_2 })))
}

async fn commands(State(dev): Shared, ConnectInfo(peer): Peer) -> Result<Json<Value>, StatusCode> {
    if peer.ip() != dev.ip {
        return Err(StatusCode::FORBIDDEN);
    }
    let (reply, more, confirm) = {
        let mut inner = dev.inner.lock().unwrap();
        if inner.session.is_none() {
            return Err(StatusCode::PRECONDITION_FAILED);
        }
        let cmd = inner.writes.pop_front().or_else(|| inner.reads.pop_front());
        for name in cmd.iter().flat_map(|c| &c.asks) {
            inner.awaiting.insert(name.clone(), Instant::now());
        }
        if let Some((name, value)) = cmd.as_ref().and_then(|c| c.applies.clone()) {
            inner.props.insert(name, value);
        }
        dev.mark_seen(&mut inner);
        inner.notified_at = None;
        inner.stalls = 0;
        // A split change is only confirmed by its last write.
        let confirm = cmd.as_ref().is_some_and(|c| c.control.is_some()) && !inner.writes.iter().any(|c| c.control.is_some());
        if confirm {
            let after = inner.next_cmd_id;
            if let Some(u) = inner.unconfirmed.as_mut() {
                u.fetched_at = Some(Instant::now());
                u.after = after;
            }
        }
        let more = inner.pending() > 0;
        let session = inner.session.as_mut().ok_or(StatusCode::PRECONDITION_FAILED)?;
        let data = cmd.map_or(json!({}), |c| c.data);
        eprintln!("[ayla] -> {data}");
        let plain = serde_json::to_vec(&json!({ "seq_no": session.seq, "data": data })).unwrap();
        session.seq += 1;
        (session.tx.seal(&plain), more, confirm)
    };
    if confirm {
        dev.queue_read(&["t_control_value"]);
    }
    if more {
        dev.wake.notify_one();
    }
    dev.changed();
    Ok(Json(reply))
}

/// Decrypts and verifies a message the unit POSTed to us.
fn open_message(dev: &Device, peer: SocketAddr, body: &Value) -> Result<Value, StatusCode> {
    if peer.ip() != dev.ip {
        return Err(StatusCode::FORBIDDEN);
    }
    let (Some(enc), Some(sign)) = (body["enc"].as_str(), body["sign"].as_str()) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let mut inner = dev.inner.lock().unwrap();
    let opened = match inner.session.as_mut() {
        Some(session) => session.rx.open(enc, sign),
        None => return Err(StatusCode::PRECONDITION_FAILED),
    };
    let plain = opened.map_err(|e| {
        eprintln!("dropping message: {e}");
        // Our CBC chain is now out of step with the unit's; force a re-key.
        inner.session = None;
        StatusCode::BAD_REQUEST
    })?;
    // Authentic but sometimes malformed (the module doesn't always escape its JSON).
    serde_json::from_slice(&plain).map_err(|e| {
        eprintln!("unparseable message ({e}): {}", String::from_utf8_lossy(&plain));
        StatusCode::BAD_REQUEST
    })
}

/// The unit echoes a read's `cmd_id` on its reply; pushes have none.
#[derive(Deserialize)]
struct ReplyQuery {
    cmd_id: Option<u64>,
}

async fn datapoint(
    State(dev): Shared,
    ConnectInfo(peer): Peer,
    Query(reply): Query<ReplyQuery>,
    Json(body): Json<Value>,
) -> StatusCode {
    let msg = match open_message(&dev, peer, &body) {
        Ok(msg) => msg,
        Err(code) => return code,
    };
    let data = &msg["data"];
    let mut report = Report::Stale;
    if let Some(name) = data["name"].as_str() {
        eprintln!("[ayla] <- {name} = {} (cmd {:?})", data["value"], reply.cmd_id);
        let mut inner = dev.inner.lock().unwrap();
        inner.awaiting.remove(name);
        if name == "t_control_value" {
            report = inner.report_control(&data["value"], reply.cmd_id);
        }
        if name == "t_beep" && !inner.muted && inner.beep_normal.is_none() {
            inner.beep_normal = data["value"].as_i64();
        }
        inner.props.insert(name.to_string(), data["value"].clone());
        dev.mark_seen(&mut inner);
    }
    match report {
        Report::Stale => {}
        Report::Recheck => dev.queue_read(&["t_control_value"]),
        Report::Settled => dev.unmute(),
    }
    dev.changed();
    StatusCode::OK
}

/// Replies to module (not property) requests, e.g. `wifi_status.json`.
async fn module_reply(
    State(dev): Shared,
    ConnectInfo(peer): Peer,
    Path(resource): Path<String>,
    Json(body): Json<Value>,
) -> StatusCode {
    let msg = match open_message(&dev, peer, &body) {
        Ok(msg) => msg,
        Err(code) => return code,
    };
    eprintln!("[ayla] <- module {resource}: {msg}");
    {
        let mut inner = dev.inner.lock().unwrap();
        inner.props.insert(format!("module:{resource}"), msg["data"].clone());
        dev.mark_seen(&mut inner);
    }
    dev.changed();
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfirmed_write_outlasts_stale_reports() {
        let (old, new) = (0x8a94_0240_u32, 0x8a98_0240_u32);
        let mut inner = Inner::default();
        inner.props.insert("t_control_value".into(), json!(old as i32));
        inner.unconfirmed = Some(Unconfirmed { value: new, fetched_at: None, after: 0, mismatches: 0 });

        assert_eq!(inner.report_control(&json!(old as i32), Some(99)), Report::Stale);
        assert_eq!(inner.control(), Some(new));

        assert_eq!(inner.report_control(&json!(new as i32), None), Report::Settled);
        assert!(inner.unconfirmed.is_none());
    }

    #[test]
    fn only_reads_issued_after_the_fetch_count_against_a_write() {
        let (old, new) = (0x8a94_0240_u32, 0x8a98_0240_u32);
        let mut inner = Inner::default();
        inner.props.insert("t_control_value".into(), json!(old as i32));
        inner.unconfirmed = Some(Unconfirmed { value: new, fetched_at: Some(Instant::now()), after: 10, mismatches: 0 });

        // Issued before the unit took the write, or an unsolicited push.
        assert_eq!(inner.report_control(&json!(old as i32), Some(9)), Report::Stale);
        assert_eq!(inner.report_control(&json!(old as i32), None), Report::Stale);

        for _ in 1..MAX_MISMATCHES {
            assert_eq!(inner.report_control(&json!(old as i32), Some(10)), Report::Recheck);
            assert_eq!(inner.control(), Some(new));
        }
        assert_eq!(inner.report_control(&json!(old as i32), Some(11)), Report::Settled);
        assert_eq!(inner.control(), Some(old));
    }

    #[test]
    fn unconfirmed_write_expires_only_after_fetch() {
        let long_ago = Instant::now() - CONFIRM_TIMEOUT - Duration::from_secs(1);
        let mut inner = Inner::default();
        inner.props.insert("t_control_value".into(), json!(1));

        inner.unconfirmed = Some(Unconfirmed { value: 2, fetched_at: None, after: 0, mismatches: 0 });
        assert!(!inner.expire_unconfirmed());
        assert_eq!(inner.control(), Some(2));

        inner.unconfirmed = Some(Unconfirmed { value: 2, fetched_at: Some(long_ago), after: 0, mismatches: 0 });
        assert!(inner.expire_unconfirmed());
        assert_eq!(inner.control(), Some(1));
    }

    #[test]
    fn seal_matches_python_reference() {
        // Vectors from the pycryptodome prototype in research/lan.py, which the unit accepts.
        let key = b"0123456789abcdef0123456789abcdef";
        let mut tx = Sender::new(key, b"abcDEF1234567890XYZ10234720217929555");
        let a = tx.seal(br#"{"seq_no":0,"data":{}}"#);
        assert_eq!(a["enc"], "+zeY/W3Cjzx2GySQKspGZZtRJfYyXuz/ZuakXGohpHs=");
        assert_eq!(a["sign"], "HQAFeb8ahmThPkVhI2lGMQ+7imZx2d9qx8F/NDTXFpU=");
        let b = tx.seal(br#"{"seq_no":1,"data":{"a":1}}"#);
        assert_eq!(b["enc"], "v0Xfr6dsxhPDlUYZtiPAn4dZJUX9k12pxuQepacD/O0=");
        assert_eq!(b["sign"], "Xh8pFLlNfV1iec1EWu3h17hwI4j+7jAATWzE5SpruVg=");
    }

    #[test]
    fn seal_then_open_roundtrips_across_messages() {
        let key = b"0123456789abcdef0123456789abcdef";
        let mut tx = Sender::new(key, b"seed");
        let mut rx = Receiver::new(key, b"seed");
        for msg in [r#"{"seq_no":0,"data":{}}"#, r#"{"seq_no":1,"data":{"name":"t_power","value":1}}"#] {
            let sealed = tx.seal(msg.as_bytes());
            let opened = rx.open(sealed["enc"].as_str().unwrap(), sealed["sign"].as_str().unwrap()).unwrap();
            assert_eq!(opened, msg.as_bytes());
        }
    }
}
