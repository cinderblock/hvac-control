//! Headless HVAC control: talks to the unit and serves the web UI.
//!
//! hvac-server [--listen ADDR:PORT] [--config PATH]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hvac_core::{ayla, config, scheduler, web};

const DEFAULT_LISTEN: &str = "0.0.0.0:48217";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hvac-server: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let mut listen = DEFAULT_LISTEN.to_string();
    let mut config_path = config::default_path();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--listen" => listen = value()?,
            "--config" => config_path = PathBuf::from(value()?),
            _ => return Err(format!("unknown argument {arg}; usage: hvac-server [--listen ADDR:PORT] [--config PATH]")),
        }
    }

    let dev = ayla::Device::new(config::load(&config_path)?)?;
    let sched = scheduler::Scheduler::new(dev.clone(), config_path.parent().unwrap_or(Path::new("")))?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("cannot listen on {listen}: {e}"))?;
    eprintln!("serving http://{listen}");
    tokio::spawn(sched.clone().run());
    tokio::select! {
        r = dev.clone().run() => r,
        r = web::serve(listener, web::router(sched)) => r,
    }
}
