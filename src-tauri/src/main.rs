#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use hvac_core::{ayla, config, scheduler, web};
use tauri::{WebviewUrl, WebviewWindowBuilder};

/// Starts the unit client and a loopback copy of the web UI; returns the UI's URL.
async fn start() -> Result<String, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let url = format!("http://{}/", listener.local_addr().map_err(|e| e.to_string())?);
    let path = config::default_path();
    let dir = path.parent().unwrap_or(std::path::Path::new(""));
    let started = config::load(&path)
        .and_then(ayla::Device::new)
        .and_then(|dev| Ok((dev.clone(), scheduler::Scheduler::new(dev, dir)?)));
    let app = match started {
        Ok((dev, sched)) => {
            tauri::async_runtime::spawn(async move {
                if let Err(e) = dev.run().await {
                    eprintln!("unit client stopped: {e}");
                }
            });
            tauri::async_runtime::spawn(sched.clone().run());
            web::router(sched)
        }
        Err(e) => web::error_router(e),
    };
    tauri::async_runtime::spawn(web::serve(listener, app));
    Ok(url)
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let url = tauri::async_runtime::block_on(start())?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
                .title("HVAC Control")
                .inner_size(380.0, 680.0)
                .min_inner_size(320.0, 520.0)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
