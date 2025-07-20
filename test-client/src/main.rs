use reqwest::Client;
use std::net::UdpSocket;
use std::time::{Duration, Instant};
use std::env;
use tokio_tungstenite::connect_async;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use serde::{Serialize, Deserialize};
use std::sync::{Arc, Mutex};
use std::thread;
use std::sync::atomic::{AtomicU64, Ordering};
use std::io::{self, Read};
use uuid::Uuid;

#[derive(Serialize, Deserialize, Debug)]
struct JitterThresholds {
    interpolate: u32,
    drop: u32,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
enum WsEvent {
    #[serde(rename = "ntp_response")]
    NtpResponse {
        t1: u64,
        t2: u64,
        t3: u64,
    },
    #[serde(rename = "heartbeat")]
    Heartbeat {
        timestamp: String,
    },
    #[serde(rename = "client_config")]
    ClientConfig {
        payload: serde_json::Value,
    },
    #[serde(rename = "request_audio")]
    RequestAudio {
        payload: serde_json::Value,
    },
    #[serde(rename = "audio_data")]
    AudioData {
        payload: serde_json::Value,
    },
    #[serde(rename = "close")]
    Close,
}

// QoS 메트릭 구조체 정의
#[derive(Serialize)]
struct QosMetricsData {
    packets_received: u32,
    packets_expected: u32,
    packet_loss_rate: f64,
    jitter_ms: f64,
    latency_ms: f64,
    bandwidth_mbytes_per_sec: f64, // MB/s
}
#[derive(Serialize)]
struct QosMetrics {
    client_id: String,
    timestamp: String,
    metrics: QosMetricsData,
}

const BACKEND_ADDR: &str = "127.0.0.1:8000";
const WS_PATH: &str = "/ws";
const LOCAL_PORT: u16 = 5004;
const CHANNELS: usize = 32;
const FRAMES: usize = 256;
const DEFAULT_JITTER_INTERPOLATE: u32 = 100;
const DEFAULT_JITTER_DROP: u32 = 200;

async fn get_jitter_thresholds() -> JitterThresholds {
    // 환경변수로 오버라이드 가능
    let interpolate = env::var("JITTER_INTERPOLATE_MS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(DEFAULT_JITTER_INTERPOLATE);
    
    let drop = env::var("JITTER_DROP_MS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(DEFAULT_JITTER_DROP);

    // 서버에서 기본값 가져오기 (실패시 환경변수 또는 기본값 사용)
    let client = Client::new();
    match client.get(&format!("http://{}/jitter-thresholds", BACKEND_ADDR)).send().await {
        Ok(resp) => {
            if let Ok(server_thresholds) = resp.json::<JitterThresholds>().await {
                println!("[JITTER] Server defaults: interpolate={}ms, drop={}ms", 
                         server_thresholds.interpolate, server_thresholds.drop);
                // 환경변수가 설정되지 않은 경우에만 서버 값 사용
                JitterThresholds {
                    interpolate: env::var("JITTER_INTERPOLATE_MS").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(server_thresholds.interpolate),
                    drop: env::var("JITTER_DROP_MS").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(server_thresholds.drop),
                }
            } else {
                println!("[JITTER] Using local defaults: interpolate={}ms, drop={}ms", interpolate, drop);
                JitterThresholds { interpolate, drop }
            }
        }
        Err(e) => {
            println!("[JITTER] Failed to get server defaults: {}, using local defaults: interpolate={}ms, drop={}ms", e, interpolate, drop);
            JitterThresholds { interpolate, drop }
        }
    }
}

async fn test_rest_apis() {
    let client = Client::new();
    // 1. /audio-devices
    match client.get(&format!("http://{}/audio-devices", BACKEND_ADDR)).send().await {
        Ok(resp) => println!("[API] /audio-devices: {}", resp.text().await.unwrap_or_default()),
        Err(e) => println!("[API] /audio-devices error: {}", e),
    }
    // 2. /audio-devices/current
    match client.get(&format!("http://{}/audio-devices/current", BACKEND_ADDR)).send().await {
        Ok(resp) => println!("[API] /audio-devices/current: {}", resp.text().await.unwrap_or_default()),
        Err(e) => println!("[API] /audio-devices/current error: {}", e),
    }
    // 3. /audio-devices/select (POST)
    match client.post(&format!("http://{}/audio-devices/select", BACKEND_ADDR))
        .json(&json!({"id": "hw:1,0"}))
        .send().await {
        Ok(resp) => println!("[API] /audio-devices/select: {}", resp.text().await.unwrap_or_default()),
        Err(e) => println!("[API] /audio-devices/select error: {}", e),
    }
    // 4. /qos-status
    match client.get(&format!("http://{}/qos-status", BACKEND_ADDR)).send().await {
        Ok(resp) => println!("[API] /qos-status: {}", resp.text().await.unwrap_or_default()),
        Err(e) => println!("[API] /qos-status error: {}", e),
    }
    // 5. /audio-meta
    match client.get(&format!("http://{}/audio-meta", BACKEND_ADDR)).send().await {
        Ok(resp) => println!("[API] /audio-meta: {}", resp.text().await.unwrap_or_default()),
        Err(e) => println!("[API] /audio-meta error: {}", e),
    }
}

#[tokio::main]
async fn main() {
    // 지터 임계값 로드
    let jitter_thresholds = get_jitter_thresholds().await;
    println!("[JITTER] Final thresholds: interpolate={}ms, drop={}ms",            jitter_thresholds.interpolate, jitter_thresholds.drop);

    test_rest_apis().await;

    let session_id = Uuid::new_v4().to_string();
    // 1. WebSocket 연결 및 client-config/heartbeat 전송 task
    let ntp_state = Arc::new(Mutex::new((0i64, 0u64))); // (offset, rtt)
    let ntp_state_clone = ntp_state.clone();
    let jitter_thresholds = Arc::new(Mutex::new(jitter_thresholds));
    let jitter_thresholds_clone = jitter_thresholds.clone();
    let ws_task = tokio::spawn(async move {
        let ws_url = format!("ws://{}{}", BACKEND_ADDR, WS_PATH);
        let (mut ws_stream, _) = connect_async(&ws_url).await.expect("WebSocket connect fail");
        println!("WebSocket connected to {}", ws_url);

        // NTP 동기화
        let t1 = chrono::Utc::now().timestamp_millis() as u64;
        let ntp_request = serde_json::json!({
            "type": "ntp_request",
            "t1": t1,
        });
        ws_stream.send(tokio_tungstenite::tungstenite::Message::Text(ntp_request.to_string())).await.unwrap();
        // NTP 응답 대기
        if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws_stream.next().await {
            if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                if let WsEvent::NtpResponse { t1, t2, t3 } = event {
                    let t4 = chrono::Utc::now().timestamp_millis() as u64;
                    let rtt = (t4 - t1).saturating_sub(t3 - t2);
                    let offset = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
                    println!("[NTP] RTT={}ms, offset={}ms (t1={}, t2={}, t3={}, t4={})", rtt, offset, t1, t2, t3, t4);
                    let mut state = ntp_state_clone.lock().unwrap();
                    *state = (offset, rtt);
                }
            }
        }

        // client-config 전송 (예시)
        let client_config = json!({
            "type": "client_config",
            "session_id": session_id,
            "payload": {
                "channels": (1..=CHANNELS).map(|id| json!({"id": id, "name": format!("CH{:02}", id), "mute": false})).collect::<Vec<_>>(),
                "sample_rate": 48000,
                "compression": "none"
            }
        });
        ws_stream.send(tokio_tungstenite::tungstenite::Message::Text(client_config.to_string())).await.unwrap();
        println!("Sent client_config");

        // /request-audio REST 호출 추가
        let client = Client::new();
        match client.get(&format!("http://{}/request-audio", BACKEND_ADDR)).send().await {
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                println!("Requested audio: {}", text);
            },
            Err(e) => println!("Failed to request audio: {}", e),
        }

        // heartbeat 주기 + 주기적 NTP 동기화
        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(5));
        let mut ntp_interval = tokio::time::interval(Duration::from_secs(30)); // 30초마다 NTP 동기화
        loop {
            tokio::select! {
                _ = heartbeat_interval.tick() => {
                    let heartbeat = json!({
                        "type": "heartbeat",
                        "session_id": session_id,
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                    });
                    ws_stream.send(tokio_tungstenite::tungstenite::Message::Text(heartbeat.to_string())).await.unwrap();
                    println!("Sent heartbeat");
                }
                _ = ntp_interval.tick() => {
                    // 주기적 NTP 동기화
                    let t1 = chrono::Utc::now().timestamp_millis() as u64;
                    let ntp_request = serde_json::json!({
                        "type": "ntp_request",
                        "t1": t1,
                    });
                    if let Err(e) = ws_stream.send(tokio_tungstenite::tungstenite::Message::Text(ntp_request.to_string())).await {
                        println!("Failed to send NTP request: {}", e);
                        break;
                    }
                    println!("Sent periodic NTP request");
                }
                Some(msg) = ws_stream.next() => {
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                                match event {
                                    WsEvent::NtpResponse { t1, t2, t3 } => {
                                        let t4 = chrono::Utc::now().timestamp_millis() as u64;
                                        let rtt = (t4 - t1).saturating_sub(t3 - t2);
                                        let offset = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
                                        println!("[NTP] Periodic sync: RTT={}ms, offset={}ms (t1={}, t2={}, t3={}, t4={})", rtt, offset, t1, t2, t3, t4);
                                        let mut state = ntp_state_clone.lock().unwrap();
                                        *state = (offset, rtt);
                                    }
                                    _ => {
                                        println!("[WS EVENT] {}", text);
                                    }
                                }
                            } else {
                                println!("[WS EVENT] {}", text);
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                            println!("WebSocket closed by server");
                            break;
                        }
                        Err(e) => {
                            println!("WebSocket error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    });

    let latency_delay_ms = Arc::new(AtomicU64::new(0));
    let latency_delay_ms_clone = latency_delay_ms.clone();
    // 키 입력 감지 task
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for byte in stdin.bytes() {
            match byte {
                Ok(b'+') => {
                    let v = latency_delay_ms_clone.fetch_add(10, Ordering::SeqCst) + 10;
                    println!("[SIM] latency_delay_ms 증가: {}ms", v);
                }
                Ok(b'-') => {
                    let v = latency_delay_ms_clone.load(Ordering::SeqCst);
                    let new_v = if v >= 10 { v - 10 } else { 0 };
                    latency_delay_ms_clone.store(new_v, Ordering::SeqCst);
                    println!("[SIM] latency_delay_ms 감소: {}ms", new_v);
                }
                _ => {}
            }
        }
    });

    // 2. RTP 수신 task (기존)
    let rtp_task = tokio::spawn(async move {
        let socket = UdpSocket::bind(("0.0.0.0", LOCAL_PORT)).expect("UDP bind fail");
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        println!("Listening for RTP on udp:{}...", LOCAL_PORT);

        let start = Instant::now();
        let mut buf = [0u8; 4096];
        let mut total_packets = 0u32;
        let mut extension_packets = 0u32;
        let mut last_latency: Option<i64> = None;
        let mut total_bytes = 0u64;
        let mut last_sample_rate: Option<u32> = None;
        let mut last_report = Instant::now();
        let mut last_seq: Option<u16> = None;
        let mut last_reported_latency = 0f64;
        let mut last_reported_jitter = 0f64;
        let mut last_reported_sample_rate = 48000u32;
        let client = Client::new();
        let client_id = format!("test-client:{}", LOCAL_PORT);
        let mut bytes_in_last_sec = 0u64;
        let mut last_rtp_report = Instant::now();
        loop {
            match socket.recv(&mut buf) {
                Ok(n) if n > 12 => {
                    total_packets += 1;
                    total_bytes += n as u64;
                    bytes_in_last_sec += n as u64;
                    let seq = u16::from_be_bytes([buf[2], buf[3]]);
                    let has_extension = (buf[0] & 0x10) != 0;
                    let recv_time = chrono::Utc::now().timestamp_millis() as i64;
                    let (ntp_offset, _ntp_rtt) = *ntp_state.lock().unwrap();
                    let mut latency = 0i64;
                    let mut sample_rate = None;
                    let mut jitter = 0i64;
                    if has_extension && n >= 32 {
                        extension_packets += 1;
                        let server_time_ms = u64::from_be_bytes([
                            buf[16], buf[17], buf[18], buf[19],
                            buf[20], buf[21], buf[22], buf[23]
                        ]);
                        sample_rate = Some(u32::from_be_bytes([
                            buf[24], buf[25], buf[26], buf[27]
                        ]));
                        latency = (recv_time + ntp_offset) - server_time_ms as i64;
                        jitter = if let Some(last) = last_latency {
                            (latency - last).abs()
                        } else { 0 };
                        last_latency = Some(latency);
                        if let Some(sr) = sample_rate {
                            last_reported_sample_rate = sr;
                        }
                        last_reported_latency = latency as f64;
                        last_reported_jitter = jitter as f64;
                    }
                    // QoS 메트릭 주기적 보고 (1초마다)
                    if last_report.elapsed() >= Duration::from_secs(1) {
                        let metrics = QosMetrics {
                            client_id: client_id.clone(),
                            session_id: session_id.clone(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            metrics: QosMetricsData {
                                packets_received: total_packets,
                                packets_expected: total_packets, // 실제로는 seq 기반 계산 필요
                                packet_loss_rate: 0.0, // 실제로는 loss 계산 필요
                                jitter_ms: last_reported_jitter,
                                latency_ms: last_reported_latency,
                                bandwidth_mbytes_per_sec: (total_bytes as f64) / (start.elapsed().as_secs_f64() * 1_048_576.0 + 1.0), // MB/s
                            },
                        };
                        let _ = client.post(&format!("http://{}/qos-metrics", BACKEND_ADDR))
                            .json(&metrics)
                            .send().await;
                        println!("[QoS] Reported to server: latency={}ms, jitter={}ms, sample_rate={}, bandwidth={:.3} MB/s", last_reported_latency, last_reported_jitter, last_reported_sample_rate, metrics.metrics.bandwidth_mbytes_per_sec);
                        last_report = Instant::now();
                    }
                    // 1초마다 수신량 출력
                    if last_rtp_report.elapsed() >= Duration::from_secs(1) {
                        let mb = bytes_in_last_sec as f64 / 1_048_576.0;
                        let cur_delay = latency_delay_ms.load(Ordering::SeqCst);
                        println!("[RTP] 1초간 수신량: {:.3} MB, sample_rate: {}, latency_delay_ms: {}ms", mb, last_reported_sample_rate, cur_delay);
                        bytes_in_last_sec = 0;
                        last_rtp_report = Instant::now();
                    }
                    last_seq = Some(seq);
                }
                Ok(_) => {
                    println!("[RTP] Received packet too small");
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // timeout, continue
                }
                Err(e) => {
                    println!("[RTP] Receive error: {}", e);
                    break;
                }
            }
        }
        println!("[RTP] Task ended. Total packets: {}, Extension packets: {}", total_packets, extension_packets);
    });

    let _ = tokio::join!(ws_task, rtp_task);
}

