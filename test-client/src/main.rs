use reqwest::Client;
use std::net::UdpSocket;
use std::time::{Duration, Instant};
use std::env;
use tokio_tungstenite::connect_async;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use serde::{Serialize, Deserialize};
use std::sync::{Arc, Mutex};

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

#[tokio::main]
async fn main() {
    // 지터 임계값 로드
    let jitter_thresholds = get_jitter_thresholds().await;
    println!("[JITTER] Final thresholds: interpolate={}ms, drop={}ms",            jitter_thresholds.interpolate, jitter_thresholds.drop);

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

    // 2. RTP 수신 task (기존)
    let rtp_task = tokio::spawn(async move {
        let socket = UdpSocket::bind(("0.0.0.0", LOCAL_PORT)).expect("UDP bind fail");
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        println!("Listening for RTP on udp:{}...", LOCAL_PORT);

        let start = Instant::now();
        let mut buf = [0u8; 4096];
        let mut total_packets = 0;
        let mut extension_packets = 0;
        let mut last_latency: Option<i64> = None;
        let mut total_bytes = 0u64; // 총 수신 바이트 수 추가
        
        while start.elapsed() < Duration::from_secs(5) {
            match socket.recv(&mut buf) {
                Ok(n) if n > 12 => {
                    total_packets += 1;
                    total_bytes += n as u64;
                    // RTP 헤더 파싱
                    let seq = u16::from_be_bytes([buf[2], buf[3]]);
                    let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                    let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                    let has_extension = (buf[0] & 0x10) != 0; // Extension 플래그 확인
                    
                    let recv_time = chrono::Utc::now().timestamp_millis() as i64;
                    let (ntp_offset, ntp_rtt) = *ntp_state.lock().unwrap();
                    
                    // Bps 계산
                    let elapsed_secs = start.elapsed().as_secs_f64();
                    let bps = if elapsed_secs > 0.0 {
                        (total_bytes as f64 / elapsed_secs) as u64
                    } else {
                        0
                    };
                    let kbps = bps as f64 / 1024.0;
                    
                    // Extension 헤더 파싱 (있는 경우)
                    let mut latency = 0i64;
                    let mut server_time_ms = 0u64;
                    
                    if has_extension && n >= 28 {
                        extension_packets += 1;
                        // Extension 헤더 파싱 (RFC3550 구조)
                        // bytes 12-13: profile (0xBEDE)
                        // bytes 14-15: length (3words = 12 bytes)
                        // bytes 16-23: server_time_ms (8 bytes)
                        // byte 24: event_flags (1 byte)
                        // bytes 25-27: reserved (3 bytes)
                        server_time_ms = u64::from_be_bytes([
                            buf[16], buf[17], buf[18], buf[19],
                            buf[20], buf[21], buf[22], buf[23]
                        ]);
                        let event_flags = buf[24];
                        
                        // latency 계산 (서버 시각 기반)
                        latency = (recv_time + ntp_offset) - server_time_ms as i64;
                        
                        // jitter 계산
                        let jitter = if let Some(last) = last_latency {
                            (latency - last).abs()
                        } else {
                            0                   };
                        last_latency = Some(latency);
                        
                        println!("[RTP] Extension packet: seq={}, server_time={}, latency={}ms, jitter={}ms, offset={}ms, rtt={}ms, bps={:.1}kbyte", seq, server_time_ms, latency, jitter, ntp_offset, ntp_rtt, kbps);
                    } else {
                        // 확장 헤더 없는 경우: 이전 값을 그대로 사용
                        let (latency, jitter) = if let Some(last) = last_latency {
                            (last, 0)
                        } else {
                            (0, 0)
                        };
                        println!(
                            "[RTP] packet: size={}, seq={}, ts=0x{:08x}, latency={}ms, jitter={}ms, offset={}ms, rtt={}ms, bps={:.1}kbyte",
                            n, seq, timestamp, latency, jitter, ntp_offset, ntp_rtt, kbps
                        );
                    }
                    
                    let payload = &buf[if has_extension { 28 } else { 12 }..n];
                    // 오디오 데이터 파싱 및 출력 코드 제거 (불필요)
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

