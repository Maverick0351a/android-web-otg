//! Binary entrypoint for `android-web-otg`.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use tokio::net::TcpListener;
use tokio::sync::watch;
use zeroize::Zeroizing;

use android_web_otg::aoa::{DeviceFilter, HidBackend, NusbAoaBridge};
use android_web_otg::cli::Cli;
use android_web_otg::server::{allowed_hosts_for_bind, build_router, AppState};
use android_web_otg::telemetry::TelemetryStore;

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Cli::parse();

    let token = match args.token {
        Some(raw) => {
            if raw.trim().is_empty() {
                eprintln!(
                    "Error: --token / ANDROID_WEB_OTG_TOKEN must not be empty or whitespace."
                );
                return ExitCode::from(2);
            }
            Some(Zeroizing::new(raw))
        }
        None => None,
    };

    let telemetry = match TelemetryStore::new(300, args.log_file) {
        Ok(store) => Arc::new(store),
        Err(err) => {
            eprintln!("Error opening telemetry --log-file: {err}");
            return ExitCode::from(2);
        }
    };

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

    if !matches!(args.host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]") && token.is_none()
    {
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

    let unlock_preset = std::env::var("ANDROID_WEB_OTG_UNLOCK_CODE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Zeroizing::new);
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

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let ignore_vids: HashSet<u16> = args.ignore_vid.into_iter().collect();
    let (bridge_impl, actor_handle) = NusbAoaBridge::spawn(
        telemetry.clone(),
        DeviceFilter { serial: args.serial, vid: args.vid, pid: args.pid, ignore_vids },
        unlock_preset,
        args.auto_unlock,
        args.unlock_wake.as_str().to_string(),
        shutdown_rx.clone(),
    );
    let bridge: Arc<dyn HidBackend> = bridge_impl;

    let allowed_hosts = allowed_hosts_for_bind(&args.host, args.port);
    let state = Arc::new(AppState {
        bridge,
        telemetry,
        token,
        sessions: Mutex::new(VecDeque::new()),
        allowed_hosts,
        shutdown_rx,
    });
    let app = build_router(state);

    let bind_target = format!("{}:{}", args.host, args.port);
    let listener = match TcpListener::bind(&bind_target).await {
        Ok(l) => l,
        Err(err) => {
            eprintln!("Failed to bind {bind_target}: {err}");
            let _ = shutdown_tx.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(2), actor_handle).await;
            return ExitCode::from(1);
        }
    };
    let actual_addr: SocketAddr =
        listener.local_addr().unwrap_or_else(|_| ([127, 0, 0, 1], args.port).into());
    println!("android-web-otg listening on http://{actual_addr}");

    let serve_res = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            println!("\nShutting down...");
            let _ = shutdown_tx.send(true);
        })
        .await;

    let _ = tokio::time::timeout(Duration::from_secs(2), actor_handle).await;

    if let Err(err) = serve_res {
        eprintln!("Server error: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
