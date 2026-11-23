//! Binary entrypoint for `android-web-otg`.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;

use android_web_otg::adb::AdbInspector;
use android_web_otg::aoa::{spawn_hotplug_watcher, DeviceFilter, HidBackend, NusbAoaBridge};
use android_web_otg::cli::{parse_hex_id, Cli};
use android_web_otg::server::{build_router, AppState};
use android_web_otg::telemetry::TelemetryStore;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Cli::parse();

    let mut ignore_vids = HashSet::new();
    for raw in &args.ignore_vid {
        match parse_hex_id(raw) {
            Ok(id) => {
                ignore_vids.insert(id);
            }
            Err(err) => {
                eprintln!("Error: {err}");
                return ExitCode::from(2);
            }
        }
    }

    let vid = match args.vid.as_deref().map(parse_hex_id).transpose() {
        Ok(v) => v,
        Err(err) => {
            eprintln!("Error: {err}");
            return ExitCode::from(2);
        }
    };
    let pid = match args.pid.as_deref().map(parse_hex_id).transpose() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("Error: {err}");
            return ExitCode::from(2);
        }
    };

    let telemetry = Arc::new(TelemetryStore::new(300, args.log_file));
    telemetry.log(
        "info",
        "system",
        format!(
            "Starting android-web-otg v{} on http://{}:{}",
            env!("CARGO_PKG_VERSION"),
            args.host,
            args.port
        ),
        None,
    );

    if !matches!(args.host.as_str(), "127.0.0.1" | "localhost" | "::1") && args.token.is_none() {
        eprintln!(
            "WARNING: Binding to non-loopback address {} without --token. \
             Only expose this over a trusted VPN/Tailscale interface or set --token.",
            args.host
        );
        telemetry.log(
            "warn",
            "system",
            format!("Non-loopback bind ({}) without --token auth", args.host),
            None,
        );
    }

    let unlock_preset = std::env::var("ANDROID_WEB_OTG_UNLOCK_CODE").ok().filter(|s| !s.is_empty());
    if args.auto_unlock && unlock_preset.is_none() {
        eprintln!(
            "WARNING: --auto-unlock set without ANDROID_WEB_OTG_UNLOCK_CODE environment variable."
        );
        telemetry.log(
            "warn",
            "system",
            "--auto-unlock set without ANDROID_WEB_OTG_UNLOCK_CODE",
            None,
        );
    }

    let bridge: Arc<dyn HidBackend> = Arc::new(NusbAoaBridge::new(
        telemetry.clone(),
        DeviceFilter { serial: args.serial.clone(), vid, pid, ignore_vids },
        unlock_preset,
        args.auto_unlock,
        args.unlock_wake.as_str().to_string(),
    ));
    spawn_hotplug_watcher(bridge.clone(), telemetry.clone());

    let adb = Arc::new(AdbInspector::new(telemetry.clone(), !args.no_adb, args.serial));
    let state = Arc::new(AppState { bridge, adb, telemetry, token: args.token });
    let app = build_router(state);

    let bind_target = format!("{}:{}", args.host, args.port);
    let listener = match TcpListener::bind(&bind_target).await {
        Ok(l) => l,
        Err(err) => {
            eprintln!("Failed to bind {bind_target}: {err}");
            return ExitCode::from(1);
        }
    };
    let actual_addr: SocketAddr =
        listener.local_addr().unwrap_or_else(|_| ([127, 0, 0, 1], args.port).into());
    println!("android-web-otg listening on http://{actual_addr}");

    let serve_res = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            println!("\nShutting down...");
        })
        .await;

    if let Err(err) = serve_res {
        eprintln!("Server error: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
