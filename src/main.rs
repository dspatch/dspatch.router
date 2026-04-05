//! dspatch-router — container-native agent router.
//!
//! Runs inside Docker containers, handling all agent routing via gRPC
//! and communicating with the host engine via a single WebSocket.

use dspatch_router::config::RouterConfig;
use dspatch_router::grpc_service::DspatchRouterService;
use dspatch_router::host_router::HostRouter;
use dspatch_router::proto::dspatch_router_server::DspatchRouterServer;
use dspatch_router::wal::Wal;
use dspatch_router::wire::WirePackage;
use dspatch_router::ws_client::WsClient;
use std::sync::Arc;
use tokio::sync::mpsc;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Logging ──
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // ── Config ──
    let config = Arc::new(RouterConfig::from_env()?);
    tracing::info!(
        run_id = %config.run_id,
        agents = ?config.agents_meta.keys().collect::<Vec<_>>(),
        "dspatch-router starting"
    );

    // ── WAL ──
    let wal = Arc::new(parking_lot::Mutex::new(Wal::open(
        std::path::Path::new(&config.wal_path),
    )?));

    // ── Host Router ──
    let host_router = Arc::new(HostRouter::new(config.agents_meta.clone()));

    // ── Channels ──
    let (engine_tx, engine_rx) = mpsc::channel::<WirePackage>(4096);
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<WirePackage>(4096);
    host_router.set_engine_tx(engine_tx.clone());

    // ── WebSocket client (engine connection) ──
    let ws_client = WsClient::new(config.clone());
    let wal_clone = wal.clone();
    tokio::spawn(async move {
        ws_client.run(engine_rx, inbound_tx, wal_clone).await;
    });

    // ── Inbound dispatcher (engine → router) ──
    let router_for_inbound = host_router.clone();
    tokio::spawn(async move {
        while let Some(pkg) = inbound_rx.recv().await {
            router_for_inbound.route_from_engine(pkg.to_json());
        }
    });

    // ── Heartbeat loop ──
    let router_for_heartbeat = host_router.clone();
    let engine_tx_hb = engine_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let instances = router_for_heartbeat.collect_heartbeat();
            let pkg = WirePackage::heartbeat(instances);
            let _ = engine_tx_hb.send(pkg).await;
        }
    });

    // ── Chain heartbeat loop (30s interval) ──
    let router_for_chain_hb = host_router.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            router_for_chain_hb.send_chain_heartbeats();
        }
    });

    // ── gRPC server ──
    let grpc_service = DspatchRouterService::new(host_router.clone());
    let addr = config.grpc_addr.parse()?;
    tracing::info!(%addr, "gRPC server listening");

    Server::builder()
        .add_service(DspatchRouterServer::new(grpc_service))
        .serve_with_shutdown(addr, async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down");
        })
        .await?;

    Ok(())
}
