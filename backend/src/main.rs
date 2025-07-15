use axum::{routing::get, Router, extract::State, Json, extract::Query};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio::task;
use tracing_subscriber;
use crate::config::{Settings, ChannelConfig};
use serde::Serialize;
use axum::serve;
use tokio::net::TcpListener;
use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;

#[derive(Clone)]
struct AppState {
    // 클라이언트별: (last_heartbeat, 송출 task 종료용 sender)
    clients: Arc<Mutex<HashMap<SocketAddr, (std::time::Instant, Option<oneshot::Sender<()>>)>>>,
    settings: Arc<Mutex<Settings>>,
}

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    compression: String,
    sample_rate: u32,
    channels: usize,
}

mod config;
mod rtp;
mod mdns;
mod audio;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let settings = config::load_config().await;
    let state = AppState {
        clients: Arc::new(Mutex::new(HashMap::new())),
        settings: Arc::new(Mutex::new(settings)),
    };

    // 설정 파일 로딩 (stub)
    // let _settings = config::load_config().await;

    // mDNS 브로드캐스트 (stub)
    task::spawn(async {
        mdns::broadcast_mdns().await;
    });

    // REST API 서버
    let app = Router::new()
        .route("/status", get(api_status))
        .route("/channels", get(api_channels))
        .route("/request-audio", get(request_audio))
        .route("/heartbeat", get(heartbeat))
        .with_state(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], 8000));
    tracing::info!("Starting server on {}", addr);
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

async fn api_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let settings = state.settings.lock().unwrap();
    Json(StatusResponse {
        status: "ok",
        compression: settings.compression.clone(),
        sample_rate: settings.sample_rate,
        channels: settings.channels.len(),
    })
}

async fn api_channels(State(state): State<AppState>) -> Json<Vec<ChannelConfig>> {
    let settings = state.settings.lock().unwrap();
    Json(settings.channels.clone())
}

async fn request_audio(
    State(state): State<AppState>,
    req: axum::extract::ConnectInfo<SocketAddr>,
    Query(params): Query<HashMap<String, String>>,
) -> &'static str {
    let client_addr = req.0;
    let udp_port = params.get("udp_port").and_then(|p| p.parse().ok()).unwrap_or(5004);
    let udp_addr = SocketAddr::new(client_addr.ip(), udp_port);
    let mut clients = state.clients.lock().unwrap();
    if clients.contains_key(&udp_addr) {
        return "already streaming";
    }
    let (tx, rx) = oneshot::channel();
    clients.insert(udp_addr, (std::time::Instant::now(), Some(tx)));
    let clients_arc = state.clients.clone();
    tokio::spawn(rtp::send_rtp_to_client(udp_addr, rx, clients_arc));
    "ok"
}

async fn heartbeat(State(state): State<AppState>, req: axum::extract::ConnectInfo<SocketAddr>) -> &'static str {
    let client_addr = req.0;
    let mut clients = state.clients.lock().unwrap();
    if let Some((last, _)) = clients.get_mut(&client_addr) {
        *last = std::time::Instant::now();
        "ok"
    } else {
        "not streaming"
    }
}
