use axum::{routing::get, Router, extract::State, Json, extract::Query};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio::task;
use tracing_subscriber;
use crate::config::{Settings, ChannelConfig, ClientConfig};
use crate::database::ChannelMapping;
use serde::{Serialize, Deserialize};
use axum::serve;
use tokio::net::TcpListener;
use axum::extract::connect_info::IntoMakeServiceWithConnectInfo;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use axum::extract;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use chrono::Utc;
use serde_json::json;
use tokio::task::JoinHandle;

// QoS 상태 추적용 메모리 맵 추가
type QosStatusMap = Arc<Mutex<HashMap<String, String>>>; // client_id -> status

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    compression: String,
    sample_rate: u32,
    channels: usize,
    channel_mappings: Vec<ChannelMapping>,
    jitter_thresholds: JitterThresholds,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum WebSocketMessage {
    #[serde(rename = "heartbeat")]
    Heartbeat {
        timestamp: String,
    },
    #[serde(rename = "client_config")]
    ClientConfig {
        payload: ClientConfig,
    },
    #[serde(rename = "get_channel_mappings")]
    GetChannelMappings,
    #[serde(rename = "update_channel_mapping")]
    UpdateChannelMapping {
        mapping: ChannelMapping,
    },
    #[serde(rename = "delete_channel_mapping")]
    DeleteChannelMapping {
        physical_channel: u8,
    },
    #[serde(rename = "reorder_channel_mappings")]
    ReorderChannelMappings {
        mappings: Vec<ChannelMapping>,
    },
    #[serde(rename = "ntp_request")]
    NtpRequest {
        t1: u64, // 클라이언트 요청 전송 시각 (ms)
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum WebSocketEvent {
    #[serde(rename = "event")]
    Event {
        payload: EventPayload,
    },
    #[serde(rename = "channel_mappings")]
    ChannelMappings {
        mappings: Vec<ChannelMapping>,
    },
    #[serde(rename = "channel_mapping_updated")]
    ChannelMappingUpdated {
        mapping: ChannelMapping,
    },
    #[serde(rename = "channel_mapping_deleted")]
    ChannelMappingDeleted {
        physical_channel: u8,
    },
    #[serde(rename = "ntp_response")]
    NtpResponse {
        t1: u64, // 클라이언트 요청 전송 시각
        t2: u64, // 서버 요청 수신 시각
        t3: u64, // 서버 응답 전송 시각
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum EventPayload {
    #[serde(rename = "config_changed")]
    ConfigChanged {
        reason: String,
        new_sample_rate: Option<u32>,
    },
    #[serde(rename = "reconnect")]
    Reconnect,
    #[serde(rename = "server_shutdown")]
    ServerShutdown,
    #[serde(rename = "network_warning")]
    NetworkWarning {
        message: String,
    },
    #[serde(rename = "time_sync")]
    TimeSync {
        server_time_ms: u64,
    },
}

#[derive(Clone)]
struct ClientSession {
    rtp_task: Option<JoinHandle<()>>,
    // 기타 상태(예: 설정, QoS 등)
}

struct AppState {
    clients: Arc<Mutex<HashMap<usize, ClientSession>>>,
    settings: Arc<Mutex<Settings>>,
    client_configs: Arc<Mutex<HashMap<String, ClientConfig>>>,
    websocket_connections: Arc<Mutex<HashMap<String, tokio::sync::broadcast::Sender<String>>>>,
    database: Arc<database::Database>,
    qos_status_map: QosStatusMap,
}

mod config;
mod rtp;
mod mdns;
mod audio;
mod database;

// QoS 상태 변화 감지 및 파일 로그 기록 함수
async fn log_qos_status_change(
    client_id: &str,
    status: &str,
    sample_rate: u32,
    latency: f64,
    jitter: f64,
) -> std::io::Result<()> {
    let now = Utc::now().to_rfc3339();
    let log_line = format!(
        "[{}] {} QoS {}: sample_rate={} latency={}ms jitter={}ms\n",
        now, client_id, status, sample_rate, latency, jitter
    );
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("qos_status.log")
        .await?;
    file.write_all(log_line.as_bytes()).await?;
    Ok(())
}

#[derive(Serialize)]
struct AudioDeviceInfo {
    id: String,
    name: String,
    is_selected: bool,
}

#[derive(Serialize)]
struct QosStatus {
    client_id: String,
    status: String,
    sample_rate: u32,
    latency_ms: f64,
    jitter_ms: f64,
    updated_at: String,
}

#[derive(Serialize)]
struct AudioMeta {
    sample_rate: u32,
    channels: usize,
    compression: String,
    channel_mappings: Vec<ChannelMapping>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // SQLite 데이터베이스 초기화
    let database = match database::Database::new().await {
        Ok(db) => {
            tracing::info!("Database initialized successfully");
            db
        }
        Err(e) => {
            tracing::error!("Failed to initialize database: {}", e);
            std::process::exit(1);
        }
    };

    // 데이터베이스에서 설정 로드
    let settings = match database.get_settings().await {
        Ok(settings) => {
            tracing::info!("Settings loaded from database: {} channels, {}Hz, {}", 
                          settings.channels.len(), settings.sample_rate, settings.compression);
            settings
        }
        Err(e) => {
            tracing::error!("Failed to load settings from database: {}", e);
            std::process::exit(1);
        }
    };

    let state = AppState {
        clients: Arc::new(Mutex::new(HashMap::new())),
        settings: Arc::new(Mutex::new(settings)),
        client_configs: Arc::new(Mutex::new(HashMap::new())),
        websocket_connections: Arc::new(Mutex::new(HashMap::new())),
        database: Arc::new(database),
        qos_status_map: Arc::new(Mutex::new(HashMap::new())),
    };

    // mDNS 브로드캐스트
    task::spawn(async {
        mdns::broadcast_mdns().await;
    });

    // REST API + WebSocket 서버
    let app = Router::new()
        .route("/status", get(|state: State<AppState>| async move {
            api_status(state).await
        }))
        .route("/channels", get(api_channels))
        .route("/request-audio", get(request_audio))
        .route("/heartbeat", get(heartbeat))
        .route("/client-config", axum::routing::post(client_config))
        .route("/ws", get(websocket_handler))
        .route("/qos-metrics", axum::routing::post(qos_metrics))
        .route("/channel-mappings", get(api_channel_mappings))
        .route("/channel-mappings", axum::routing::post(api_create_channel_mapping))
        .route("/channel-mappings/:physical_channel", axum::routing::put(api_update_channel_mapping))
        .route("/channel-mappings/:physical_channel", axum::routing::delete(api_delete_channel_mapping))
        .route("/channel-mappings/reorder", axum::routing::post(api_reorder_channel_mappings))
        .route("/jitter-thresholds", get(api_get_jitter_thresholds))
        .route("/jitter-thresholds", axum::routing::post(api_set_jitter_thresholds))
        .route("/audio-devices", get(api_audio_devices))
        .route("/audio-devices/select", axum::routing::post(api_audio_device_select))
        .route("/audio-devices/current", get(api_audio_device_current))
        .route("/qos-status", get(api_qos_status))
        .route("/audio-meta", get(|state: State<AppState>| async move { api_audio_meta(state).await }))
        .with_state(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], 8000));
    tracing::info!("Starting server on {}", addr);
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind to {}: {}", addr, e);
            return;
        }
    };
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        tracing::error!("Server error: {}", e);
    }
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    req: axum::extract::ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let client_addr = req.0;
    tracing::info!("WebSocket connection from {}", client_addr);
    
    ws.on_upgrade(move |socket| handle_websocket(socket, state, client_addr))
}

async fn handle_websocket(socket: WebSocket, state: AppState, client_addr: SocketAddr) {
    let (mut sender, mut receiver) = socket.split();
    
    // 클라이언트별 이벤트 브로드캐스트 채널 생성
    let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(100);
    let tx = std::sync::Arc::new(tx);
    let tx_event = tx.clone();
    let tx_message = tx.clone();
    let websocket_connections = state.websocket_connections.clone();
    let client_addr_event = client_addr.to_string();
    let client_addr_message = client_addr.to_string();
    
    // 이벤트 수신 태스크
    let event_task = tokio::spawn(async move {
        // 주기적으로 시각 전송 task
        let tx_time = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let now = chrono::Utc::now().timestamp_millis() as u64;
                let event = WebSocketEvent::Event {
                    payload: EventPayload::TimeSync { server_time_ms: now },
                };
                if let Ok(event_json) = serde_json::to_string(&event) {
                    let _ = tx_time.send(event_json);
                }
            }
        });
        while let Ok(event_json) = rx.recv().await {
            if let Err(e) = sender.send(axum::extract::ws::Message::Text(event_json)).await {
                tracing::error!("Failed to send event to {}: {}", client_addr_event, e);
                break;
            }
        }
    });
    
    // 메시지 수신 태스크
    let message_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(axum::extract::ws::Message::Text(text)) => {
                    if let Ok(message) = serde_json::from_str::<WebSocketMessage>(&text) {
                        match message {
                            WebSocketMessage::Heartbeat { timestamp } => {
                                tracing::info!("Heartbeat from {}: {}", client_addr_message, timestamp);
                                // heartbeat 업데이트
                                let mut clients = state.clients.lock().unwrap();
                                if let Some((last_heartbeat, _)) = clients.get_mut(&client_addr_message) {
                                    *last_heartbeat = std::time::Instant::now();
                                }
                            }
                            WebSocketMessage::ClientConfig { payload } => {
                                tracing::info!("Client config from {}: {:?}", client_addr_message, payload);
                                // client-config를 데이터베이스에 저장
                                if let Err(e) = state.database.save_client_config(
                                    &client_addr_message,
                                    &payload
                                ).await {
                                    tracing::error!("Failed to save client config: {}", e);
                                }
                                // 메모리에도 저장
                                let mut configs = state.client_configs.lock().unwrap();
                                configs.insert(client_addr_message, payload);
                            }
                            WebSocketMessage::GetChannelMappings => {
                                tracing::info!("Channel mappings request from {}", client_addr_message);
                                match state.database.get_channel_mappings().await {
                                    Ok(mappings) => {
                                        let event = WebSocketEvent::ChannelMappings { mappings };
                                        if let Ok(event_json) = serde_json::to_string(&event) {
                                            if let Err(e) = tx_event.send(event_json) {
                                                tracing::error!("Failed to send channel mappings: {}", e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to get channel mappings: {}", e);
                                    }
                                }
                            }
                            WebSocketMessage::UpdateChannelMapping { mapping } => {
                                tracing::info!("Update channel mapping from {}: {:?}", client_addr_message, mapping);
                                match state.database.update_channel_mapping(&mapping).await {
                                    Ok(_) => {
                                        // 설정 변경 로그
                                        if let Err(e) = state.database.log_config_change(
                                            "channel_mapping_update",
                                            "websocket_request",
                                            &format!("Updated mapping for physical channel {}", mapping.physical_channel)
                                        ).await {
                                            tracing::error!("Failed to log config change: {}", e);
                                        }
                                        
                                        // 모든 WebSocket 클라이언트에게 이벤트 브로드캐스트
                                        let event = WebSocketEvent::ChannelMappingUpdated { mapping: mapping.clone() };
                                        if let Ok(event_json) = serde_json::to_string(&event) {
                                            let connections = websocket_connections.lock().unwrap();
                                            for (_, tx) in connections.iter() {
                                                if let Err(e) = tx.send(event_json.clone()) {
                                                    tracing::debug!("Failed to broadcast event: {}", e);
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to update channel mapping: {}", e);
                                    }
                                }
                            }
                            WebSocketMessage::DeleteChannelMapping { physical_channel } => {
                                tracing::info!("Delete channel mapping from {}: physical_channel={}", client_addr_message, physical_channel);
                                match state.database.delete_channel_mapping(physical_channel).await {
                                    Ok(_) => {
                                        // 설정 변경 로그
                                        if let Err(e) = state.database.log_config_change(
                                            "channel_mapping_delete",
                                            "websocket_request",
                                            &format!("Deleted mapping for physical channel {}", physical_channel)
                                        ).await {
                                            tracing::error!("Failed to log config change: {}", e);
                                        }
                                        
                                        // 모든 WebSocket 클라이언트에게 이벤트 브로드캐스트
                                        let event = WebSocketEvent::ChannelMappingDeleted { physical_channel };
                                        if let Ok(event_json) = serde_json::to_string(&event) {
                                            let connections = websocket_connections.lock().unwrap();
                                            for (_, tx) in connections.iter() {
                                                if let Err(e) = tx.send(event_json.clone()) {
                                                    tracing::debug!("Failed to broadcast event: {}", e);
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to delete channel mapping: {}", e);
                                    }
                                }
                            }
                            WebSocketMessage::ReorderChannelMappings { mappings } => {
                                tracing::info!("Reorder channel mappings from {}: {} mappings", client_addr_message, mappings.len());
                                match state.database.reorder_channel_mappings(&mappings).await {
                                    Ok(_) => {
                                        // 설정 변경 로그
                                        if let Err(e) = state.database.log_config_change(
                                            "channel_mapping_reorder",
                                            "websocket_request",
                                            &format!("Reordered {} channel mappings", mappings.len())
                                        ).await {
                                            tracing::error!("Failed to log config change: {}", e);
                                        }
                                        
                                        // 모든 WebSocket 클라이언트에게 이벤트 브로드캐스트
                                        let event = WebSocketEvent::ChannelMappings { mappings: mappings.clone() };
                                        if let Ok(event_json) = serde_json::to_string(&event) {
                                            let connections = websocket_connections.lock().unwrap();
                                            for (_, tx) in connections.iter() {
                                                if let Err(e) = tx.send(event_json.clone()) {
                                                    tracing::debug!("Failed to broadcast event: {}", e);
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to reorder channel mappings: {}", e);
                                    }
                                }
                            }
                            WebSocketMessage::NtpRequest { t1 } => {
                                let t2 = chrono::Utc::now().timestamp_millis() as u64;
                                let t3 = chrono::Utc::now().timestamp_millis() as u64;
                                let response = WebSocketEvent::NtpResponse { t1, t2, t3 };
                                if let Ok(event_json) = serde_json::to_string(&response) {
                                    if let Err(e) = tx_event.send(event_json) {
                                        tracing::error!("Failed to send NTP response: {}", e);
                                    }
                                }
                                tracing::info!("[NTP] NTP request handled: t1={}, t2={}, t3={}", t1, t2, t3);
                            }
                        }
                    } else {
                        tracing::warn!("Invalid WebSocket message from {}: {}", client_addr_message, text);
                    }
                }
                Ok(axum::extract::ws::Message::Close(_)) => {
                    tracing::info!("WebSocket connection closed by {}", client_addr_message);
                    break;
                }
                Err(e) => {
                    tracing::error!("WebSocket error from {}: {}", client_addr_message, e);
                    break;
                }
                _ => {}
            }
        }
    });
    
    // 태스크 완료 대기
    tokio::select! {
        _ = event_task => tracing::info!("Event task ended for {}", client_addr_event),
        _ = message_task => tracing::info!("Message task ended for {}", client_addr_message),
    }
    
    // 연결 정리
    {
        let mut connections = state.websocket_connections.lock().unwrap();
        connections.remove(&client_addr_event);
    }
    tracing::info!("WebSocket connection cleaned up for {}", client_addr_event);
}

async fn api_status(
    State(state): State<AppState>,
) -> Json<StatusResponse> {
    // 필요한 값만 복사
    let (compression, sample_rate, channels) = {
        let settings = state.settings.lock().unwrap();
        (
            settings.compression.clone(),
            settings.sample_rate,
            settings.channels.len(),
        )
    }; // MutexGuard drop

    let channel_mappings = match state.database.get_active_channel_mappings().await {
        Ok(mappings) => mappings,
        Err(e) => {
            tracing::error!("Failed to get channel mappings: {}", e);
            Vec::new()
        }
    };
    let jitter_thresholds = match state.database.get_jitter_thresholds().await {
        Ok((interpolate, drop)) => JitterThresholds { interpolate, drop },
        Err(_) => JitterThresholds { interpolate: 100, drop: 200 },
    };
    Json(StatusResponse {
        status: "running",
        compression,
        sample_rate,
        channels,
        channel_mappings,
        jitter_thresholds,
    })
}

async fn api_channels(
    State(state): State<AppState>,
) -> Json<Vec<ChannelConfig>> {
    let settings = state.settings.lock().unwrap();
    Json(settings.channels.clone())
}

async fn request_audio(
    State(state): State<AppState>,
    req: axum::extract::ConnectInfo<SocketAddr>,
    Query(_params): Query<HashMap<String, String>>,
) -> &'static str {
    let client_addr = req.0;
    tracing::info!("Audio request from {}", client_addr);
    
    // 클라이언트가 이미 스트리밍 중인지 확인
    {
        let clients = state.clients.lock().unwrap();
        if clients.contains_key(&client_addr.to_string()) {
            tracing::info!("Client {} already streaming, continuing", client_addr);
            return "streaming";
        }
    }
    
    // 클라이언트 설정 가져오기
    let client_config = {
        let configs = state.client_configs.lock().unwrap();
        configs.get(&client_addr.to_string()).cloned()
    };
    
    // 채널 매핑 적용
    let active_mappings = match state.database.get_active_channel_mappings().await {
        Ok(mappings) => mappings,
        Err(e) => {
            tracing::error!("Failed to get channel mappings: {}", e);
            return "error";
        }
    };
    
    // 활성화된 채널 수 계산
    let active_channels = active_mappings.iter().filter(|m| m.enabled).count();
    if active_channels == 0 {
        tracing::warn!("No active channels found");
        return "no_channels";
    }
    
    // 클라이언트 상태에 추가
    let (tx, rx) = oneshot::channel();
    {
        let mut clients = state.clients.lock().unwrap();
        clients.insert(client_addr.to_string(), (std::time::Instant::now(), Some(tx)));
    }
    
    // RTP 스트리밍 시작 (채널 매핑 정보 전달)
    let clients = state.clients.clone();
    let database = state.database.clone();
    task::spawn(async move {
        rtp::send_rtp_to_client_with_mapping(
            client_addr.to_string(),
            rx,
            clients,
            client_config,
            database,
        ).await;
    });
    
    "started"
}

async fn heartbeat(
    State(state): State<AppState>,
    req: axum::extract::ConnectInfo<SocketAddr>,
) -> &'static str {
    let client_addr = req.0;
    let mut clients = state.clients.lock().unwrap();
    if let Some((last_heartbeat, _)) = clients.get_mut(&client_addr.to_string()) {
        *last_heartbeat = std::time::Instant::now();
    }
    "ok"
}

async fn client_config(
    State(state): State<AppState>,
    req: axum::extract::ConnectInfo<SocketAddr>,
    Json(cfg): Json<ClientConfig>,
) -> &'static str {
    let client_addr = req.0;
    // 데이터베이스에 저장
    if let Err(e) = state.database.save_client_config(
        &client_addr.to_string(),
        &cfg
    ).await {
        tracing::error!("Failed to save client config: {}", e);
        return "error";
    }
    // 메모리에도 저장
    let mut configs = state.client_configs.lock().unwrap();
    configs.insert(client_addr.to_string(), cfg);
    "ok"
}

#[derive(Debug, Deserialize)]
struct QosMetrics {
    client_id: String,
    timestamp: String,
    metrics: QosMetricsData,
}

#[derive(Debug, Deserialize)]
struct QosMetricsData {
    packets_received: u32,
    packets_expected: u32,
    packet_loss_rate: f64,
    jitter_ms: f64,
    latency_ms: f64,
    bandwidth_mbps: f64,
}

async fn qos_metrics(
    State(state): State<AppState>,
    req: axum::extract::ConnectInfo<SocketAddr>,
    Json(qos): Json<QosMetrics>,
) -> Json<serde_json::Value> {
    let client_addr = req.0;
    let client_id = client_addr.to_string();
    tracing::info!("QoS metrics from {}: {:?}", client_addr, qos);
    
    // QoS 메트릭을 데이터베이스에 저장
    if let Err(e) = state.database.save_qos_metrics(
        &client_id,
        qos.metrics.packets_received,
        qos.metrics.packets_expected,
        qos.metrics.packet_loss_rate,
        qos.metrics.jitter_ms,
        qos.metrics.latency_ms,
        qos.metrics.bandwidth_mbps,
    ).await {
        tracing::error!("Failed to save QoS metrics: {}", e);
    }
    
    // QoS 상태 변화 감지 및 로그 기록
    let status = if qos.metrics.jitter_ms > 50.0 || qos.metrics.latency_ms > 50.0 {
        "degraded"
    } else {
        "normal"
    };
    let mut qos_map = state.qos_status_map.lock().unwrap();
    let prev_status = qos_map.get(&client_id).cloned();
    if prev_status.as_deref() != Some(status) {
        // sample_rate: client_configs에서 우선 조회, 없으면 settings에서 가져옴
        let sample_rate = {
            let configs = state.client_configs.lock().unwrap();
            if let Some(cfg) = configs.get(&client_id) {
                cfg.sample_rate.unwrap_or_else(|| {
                    let settings = state.settings.lock().unwrap();
                    settings.sample_rate
                })
            } else {
                let settings = state.settings.lock().unwrap();
                settings.sample_rate
            }
        };
        if let Err(e) = log_qos_status_change(
            &client_id,
            status,
            sample_rate,
            qos.metrics.latency_ms,
            qos.metrics.jitter_ms,
        )
        .await
        {
            tracing::error!("Failed to log QoS status change: {}", e);
        }
        qos_map.insert(client_id.clone(), status.to_string());
    }
    Json(serde_json::json!({
        "status": "received",
        "client": client_addr.to_string(),
        "timestamp": qos.timestamp
    }))
}

// 채널 매핑 API 핸들러들
async fn api_channel_mappings(
    State(state): State<AppState>,
) -> Json<Vec<ChannelMapping>> {
    match state.database.get_channel_mappings().await {
        Ok(mappings) => Json(mappings),
        Err(e) => {
            tracing::error!("Failed to get channel mappings: {}", e);
            Json(Vec::new())
        }
    }
}

async fn api_create_channel_mapping(
    State(state): State<AppState>,
    Json(mapping): Json<ChannelMapping>,
) -> Json<serde_json::Value> {
    tracing::info!("Creating channel mapping: {:?}", mapping);
    
    match state.database.update_channel_mapping(&mapping).await {
        Ok(_) => {
            // 설정 변경 로그
            if let Err(e) = state.database.log_config_change(
                "channel_mapping_create",
                "rest_api",
                &format!("Created mapping for physical channel {}", mapping.physical_channel)
            ).await {
                tracing::error!("Failed to log config change: {}", e);
            }
            
            // WebSocket 클라이언트들에게 이벤트 브로드캐스트
            let event = WebSocketEvent::ChannelMappingUpdated { mapping: mapping.clone() };
            if let Ok(event_json) = serde_json::to_string(&event) {
                let connections = state.websocket_connections.lock().unwrap();
                for (_, tx) in connections.iter() {
                    if let Err(e) = tx.send(event_json.clone()) {
                        tracing::debug!("Failed to broadcast event: {}", e);
                    }
                }
            }
            
            Json(serde_json::json!({
                "status": "created",
                "mapping": mapping
            }))
        }
        Err(e) => {
            tracing::error!("Failed to create channel mapping: {}", e);
            Json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            }))
        }
    }
}

async fn api_update_channel_mapping(
    State(state): State<AppState>,
    axum::extract::Path(physical_channel): axum::extract::Path<u8>,
    Json(mapping): Json<ChannelMapping>,
) -> Json<serde_json::Value> {
    tracing::info!("Updating channel mapping for physical channel {}: {:?}", physical_channel, mapping);
    
    // URL 경로의 physical_channel과 요청 본문의 physical_channel이 일치하는지 확인
    if mapping.physical_channel != physical_channel {
        return Json(serde_json::json!({
            "status": "error",
            "message": "Physical channel mismatch"
        }));
    }
    
    match state.database.update_channel_mapping(&mapping).await {
        Ok(_) => {
            // 설정 변경 로그
            if let Err(e) = state.database.log_config_change(
                "channel_mapping_update",
                "rest_api",
                &format!("Updated mapping for physical channel {}", physical_channel)
            ).await {
                tracing::error!("Failed to log config change: {}", e);
            }
            
            // WebSocket 클라이언트들에게 이벤트 브로드캐스트
            let event = WebSocketEvent::ChannelMappingUpdated { mapping: mapping.clone() };
            if let Ok(event_json) = serde_json::to_string(&event) {
                let connections = state.websocket_connections.lock().unwrap();
                for (_, tx) in connections.iter() {
                    if let Err(e) = tx.send(event_json.clone()) {
                        tracing::debug!("Failed to broadcast event: {}", e);
                    }
                }
            }
            
            Json(serde_json::json!({
                "status": "updated",
                "mapping": mapping
            }))
        }
        Err(e) => {
            tracing::error!("Failed to update channel mapping: {}", e);
            Json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            }))
        }
    }
}

async fn api_delete_channel_mapping(
    State(state): State<AppState>,
    axum::extract::Path(physical_channel): axum::extract::Path<u8>,
) -> Json<serde_json::Value> {
    tracing::info!("Deleting channel mapping for physical channel {}", physical_channel);
    
    match state.database.delete_channel_mapping(physical_channel).await {
        Ok(_) => {
            // 설정 변경 로그
            if let Err(e) = state.database.log_config_change(
                "channel_mapping_delete",
                "rest_api",
                &format!("Deleted mapping for physical channel {}", physical_channel)
            ).await {
                tracing::error!("Failed to log config change: {}", e);
            }
            
            // WebSocket 클라이언트들에게 이벤트 브로드캐스트
            let event = WebSocketEvent::ChannelMappingDeleted { physical_channel };
            if let Ok(event_json) = serde_json::to_string(&event) {
                let connections = state.websocket_connections.lock().unwrap();
                for (_, tx) in connections.iter() {
                    if let Err(e) = tx.send(event_json.clone()) {
                        tracing::debug!("Failed to broadcast event: {}", e);
                    }
                }
            }
            
            Json(serde_json::json!({
                "status": "deleted",
                "physical_channel": physical_channel
            }))
        }
        Err(e) => {
            tracing::error!("Failed to delete channel mapping: {}", e);
            Json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            }))
        }
    }
}

async fn api_reorder_channel_mappings(
    State(state): State<AppState>,
    Json(mappings): Json<Vec<ChannelMapping>>,
) -> Json<serde_json::Value> {
    tracing::info!("Reordering {} channel mappings", mappings.len());
    
    match state.database.reorder_channel_mappings(&mappings).await {
        Ok(_) => {
            // 설정 변경 로그
            if let Err(e) = state.database.log_config_change(
                "channel_mapping_reorder",
                "rest_api",
                &format!("Reordered {} channel mappings", mappings.len())
            ).await {
                tracing::error!("Failed to log config change: {}", e);
            }
            
            // WebSocket 클라이언트들에게 이벤트 브로드캐스트
            let event = WebSocketEvent::ChannelMappings { mappings: mappings.clone() };
            if let Ok(event_json) = serde_json::to_string(&event) {
                let connections = state.websocket_connections.lock().unwrap();
                for (_, tx) in connections.iter() {
                    if let Err(e) = tx.send(event_json.clone()) {
                        tracing::debug!("Failed to broadcast event: {}", e);
                    }
                }
            }
            
            Json(serde_json::json!({
                "status": "reordered",
                "count": mappings.len()
            }))
        }
        Err(e) => {
            tracing::error!("Failed to reorder channel mappings: {}", e);
            Json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            }))
        }
    }
}

#[derive(Serialize, Deserialize)]
struct JitterThresholds {
    interpolate: u32,
    drop: u32,
}

async fn api_get_jitter_thresholds(State(state): State<AppState>) -> Json<JitterThresholds> {
    let (interpolate, drop) = match state.database.get_jitter_thresholds().await {
        Ok((i, d)) => (i, d),
        Err(_) => (100, 200),
    };
    Json(JitterThresholds { interpolate, drop })
}

async fn api_set_jitter_thresholds(State(state): State<AppState>, Json(thresholds): Json<JitterThresholds>) -> Json<serde_json::Value> {
    let res = state.database.update_jitter_thresholds(thresholds.interpolate, thresholds.drop).await;
    match res {
        Ok(_) => Json(serde_json::json!({ "status": "ok" })),
        Err(e) => Json(serde_json::json!({ "status": "error", "message": e.to_string() })),
    }
}

// GET /audio-devices
async fn api_audio_devices(State(state): State<AppState>) -> Json<serde_json::Value> {
    // mock: 2개 장치, 1개 선택
    let devices = vec![
        AudioDeviceInfo { id: "hw:0,0".to_string(), name: "M32 USB".to_string(), is_selected: true },
        AudioDeviceInfo { id: "hw:1,0".to_string(), name: "Generic USB".to_string(), is_selected: false },
    ];
    Json(json!({ "devices": devices, "selected_device": "hw:0,0" }))
}

// POST /audio-devices/select
async fn api_audio_device_select(State(state): State<AppState>, Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
    // mock: 선택 성공
    let device_id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("");
    Json(json!({ "result": "ok", "selected_device": device_id }))
}

// GET /audio-devices/current
async fn api_audio_device_current(State(state): State<AppState>) -> Json<AudioDeviceInfo> {
    // mock: 현재 선택된 장치
    Json(AudioDeviceInfo { id: "hw:0,0".to_string(), name: "M32 USB".to_string(), is_selected: true })
}

// GET /qos-status
async fn api_qos_status(State(state): State<AppState>) -> Json<Vec<QosStatus>> {
    // mock: 2개 클라이언트 QoS 상태
    let now = chrono::Utc::now().to_rfc3339();
    let list = vec![
        QosStatus { client_id: "client_001".to_string(), status: "normal".to_string(), sample_rate: 48000, latency_ms: 20.0, jitter_ms: 2.0, updated_at: now.clone() },
        QosStatus { client_id: "client_002".to_string(), status: "degraded".to_string(), sample_rate: 32000, latency_ms: 95.0, jitter_ms: 7.0, updated_at: now },
    ];
    Json(list)
}

// GET /audio-meta
async fn api_audio_meta(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    let (sample_rate, channels, compression) = {
        let settings = state.settings.lock().unwrap();
        (settings.sample_rate, settings.channels.len(), settings.compression.clone())
    };
    let channel_mappings = match state.database.get_active_channel_mappings().await {
        Ok(mappings) => mappings,
        Err(_) => Vec::new(),
    };
    Json(AudioMeta {
        sample_rate,
        channels,
        compression,
        channel_mappings,
    })
}
