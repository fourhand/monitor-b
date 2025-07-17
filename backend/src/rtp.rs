use crate::audio;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use crate::config::ClientConfig;
use crate::database::Database;

const RTP_VERSION: u8 = 2;
const RTP_PAYLOAD_TYPE: u8 = 96; // dynamic
const RTP_SSRC: u32 = 0x12345678;
const RTP_PORT: u16 = 5004;
const EXTENSION_PROFILE: u16 = 0xBEDE; // RFC3550 표준 프로파일
const EXTENSION_INTERVAL: u32 = 250; // 5마다 Extension (20 *250 = 5000ms)

// RTP Extension 헤더 구조 (RFC3550)
#[derive(Debug)]
struct RtpExtension {
    profile: u16,        // 0BEDE (표준 프로파일)
    length: u16,         // Extension 데이터 길이 (워드 단위)
    server_time_ms: u64, // 서버 UTC 시각 (ms)
    event_flags: u8,    // 이벤트 플래그
    reserved: [u8; 3],   // 예약된 바이트
}

impl RtpExtension {
    fn new_time_sync() -> Self {
        Self {
            profile: EXTENSION_PROFILE,
            length: 3, // 3 words
            server_time_ms: chrono::Utc::now().timestamp_millis() as u64,
            event_flags: 0b00000001, // time_sync flag
            reserved: [0; 3],
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&self.profile.to_be_bytes());
        bytes.extend_from_slice(&self.length.to_be_bytes());
        bytes.extend_from_slice(&self.server_time_ms.to_be_bytes());
        bytes.push(self.event_flags);
        bytes.extend_from_slice(&self.reserved);
        bytes
    }
}

fn build_rtp_header(seq: u16, timestamp: u32, ssrc: u32, has_extension: bool) -> Vec<u8> {
    let mut header = vec![0u8; 12];
    unsafe {
        let ptr = header.as_mut_ptr();
        if has_extension {
            *ptr.add(0) = (RTP_VERSION << 6) | 0b10; // V=2, X=1, CC=0
        } else {
            *ptr.add(0) = (RTP_VERSION << 6) | 0; // V=2, X=0
        }
        *ptr.add(1) = RTP_PAYLOAD_TYPE & 0x7F; // M=0, PT=96
        *ptr.add(2) = (seq >> 8) as u8;
        *ptr.add(3) = (seq & 0xFF) as u8;
        *ptr.add(4) = (timestamp >> 24) as u8;
        *ptr.add(5) = (timestamp >> 16) as u8;
        *ptr.add(6) = (timestamp >> 8) as u8;
        *ptr.add(7) = (timestamp & 0xFF) as u8;
        *ptr.add(8) = (ssrc >> 24) as u8;
        *ptr.add(9) = (ssrc >> 16) as u8;
        *ptr.add(10) = (ssrc >> 8) as u8;
        *ptr.add(11) = (ssrc & 0xFF) as u8;
    }
    header
}

pub async fn send_rtp_to_client(
    client_addr: SocketAddr,
    mut stop_rx: oneshot::Receiver<()>,
    clients: Arc<Mutex<HashMap<SocketAddr, (Instant, Option<oneshot::Sender<()>>)>>>,
    config: Option<ClientConfig>,
) {
    // config가 있으면 우선 적용, 없으면 기본값
    let channels = config.as_ref().and_then(|c| c.channels.as_ref()).map(|v| v.len()).unwrap_or(2);
    let frames = 160; // frames도 config에서 받을 수 있음
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let socket = UdpSocket::bind("0.0.0.0:0").await.expect("UDP bind failed");
    let mut sent_packets = 0u64;
    let mut send_errors = 0u64;
    loop {
        // heartbeat 체크
        let last_heartbeat = {
            let clients = clients.lock().unwrap();
            clients.get(&client_addr).map(|(t, _)| *t)
        };
        if let Some(last) = last_heartbeat {
            if last.elapsed() > Duration::from_secs(30) {
                tracing::info!("Client {}: heartbeat timeout, stopping RTP", client_addr);
                break;
            }
        } else {
            tracing::info!("Client {}: not found in state, stopping RTP", client_addr);
            break;
        }
        tokio::select! {
            _ = &mut stop_rx => {
                tracing::info!("Client {}: stop signal received", client_addr);
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                let audio_frame = audio::capture_audio_frame(channels, frames).await;
                let hdr = build_rtp_header(seq, timestamp, RTP_SSRC, false);
                let mut payload = Vec::with_capacity(audio_frame.len() * 4);
                for &s in &audio_frame {
                    payload.extend_from_slice(&s.to_le_bytes());
                }
                let mut packet = Vec::with_capacity(12 + payload.len());
                packet.extend_from_slice(&hdr);
                packet.extend_from_slice(&payload);
                match socket.send_to(&packet, &client_addr).await {
                    Ok(_) => sent_packets += 1,
                    Err(e) => {
                        send_errors += 1;
                        tracing::warn!("RTP send error to {}: {}", client_addr, e);
                    }
                }
                seq = seq.wrapping_add(1);
                timestamp = timestamp.wrapping_add(frames as u32);
                if sent_packets % 100 == 0 {
                    tracing::info!("RTP stats for {}: sent={}, errors={}", client_addr, sent_packets, send_errors);
                }
            }
        }
    }
    tracing::info!("RTP task for {} ended. Total sent={}, errors={}", client_addr, sent_packets, send_errors);
    // Remove client from state to allow reconnect
    let mut clients = clients.lock().unwrap();
    clients.remove(&client_addr);
}

pub async fn send_rtp_to_client_with_mapping(
    client_addr: SocketAddr,
    mut stop_rx: oneshot::Receiver<()>,
    clients: Arc<Mutex<HashMap<SocketAddr, (Instant, Option<oneshot::Sender<()>>)>>>,
    config: Option<ClientConfig>,
    database: Arc<Database>,
) {
    // 클라이언트 설정에서 채널 수 결정
    let client_channels = config.as_ref().and_then(|c| c.channels.as_ref()).map(|v| v.len()).unwrap_or(32);
    
    // 활성화된 채널 매핑 가져오기
    let active_mappings = match database.get_active_channel_mappings().await {
        Ok(mappings) => mappings,
        Err(e) => {
            tracing::error!("Failed to get channel mappings for {}: {}", client_addr, e);
            return;
        }
    };
    
    // 활성화된 채널 수 계산
    let active_channels = active_mappings.iter().filter(|m| m.enabled).count();
    if active_channels == 0 {
        tracing::warn!("No active channels found for {}", client_addr);
        return;
    }
    
    // 클라이언트가 요청한 채널 수와 활성화된 채널 수 중 작은 값 사용
    let channels = std::cmp::min(client_channels, active_channels);
    let frames = 160;
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let socket = UdpSocket::bind("0.0.0.0:0").await.expect("UDP bind failed");
    let mut sent_packets = 0u64;
    let mut send_errors = 0u64;
    let mut extension_counter = 0u32; // Extension 카운터 추가
    
    tracing::info!("Starting RTP streaming to {} with {} channels ({} active mappings)", 
                   client_addr, channels, active_channels);
    
    loop {
        // heartbeat 체크
        let last_heartbeat = {
            let clients = clients.lock().unwrap();
            clients.get(&client_addr).map(|(t, _)| *t)
        };
        if let Some(last) = last_heartbeat {
            if last.elapsed() > Duration::from_secs(30) {
                tracing::info!("Client {}: heartbeat timeout, stopping RTP", client_addr);
                break;
            }
        } else {
            tracing::info!("Client {}: not found in state, stopping RTP", client_addr);
            break;
        }
        
        tokio::select! {
            _ = &mut stop_rx => {
                tracing::info!("Client {}: stop signal received", client_addr);
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                // Extension 헤더 포함 여부 결정
                let has_extension = extension_counter % EXTENSION_INTERVAL == 0;
                if has_extension {
                    tracing::debug!("Sending extension header to {}, extension_counter={}", client_addr, extension_counter);
                }
                
                // 원본 오디오 프레임 캡처 (모든 물리적 채널)
                let original_frame = audio::capture_audio_frame(32, frames).await;
                
                // 채널 매핑을 적용하여 논리적 채널 순서로 재배열
                let mut mapped_frame = Vec::with_capacity(channels * frames);
                
                for i in 0..channels {
                    if let Some(mapping) = active_mappings.get(i) {
                        if mapping.enabled {
                            // 물리적 채널에서 데이터 가져오기
                            let physical_channel = mapping.physical_channel as usize - 1; // 0-based index
                            if physical_channel < 32 {
                                let start_idx = physical_channel * frames;
                                let end_idx = start_idx + frames;
                                if end_idx <= original_frame.len() {
                                    mapped_frame.extend_from_slice(&original_frame[start_idx..end_idx]);
                                } else {
                                    // 채널이 없으면 0으로 채움
                                    mapped_frame.extend(std::iter::repeat(0).take(frames));
                                }
                            } else {
                                // 잘못된 물리적 채널 번호면 0으로 채움
                                mapped_frame.extend(std::iter::repeat(0).take(frames));
                            }
                        } else {
                            // 비활성화된 채널이면 0으로 채움
                            mapped_frame.extend(std::iter::repeat(0).take(frames));
                        }
                    } else {
                        // 매핑이 없으면 0으로 채움
                        mapped_frame.extend(std::iter::repeat(0).take(frames));
                    }
                }
                
                // RTP 패킷 생성
                let mut payload = Vec::with_capacity(mapped_frame.len() * 4);
                for &s in &mapped_frame {
                    payload.extend_from_slice(&s.to_le_bytes());
                }

                // RTP 패킷 분할(chunking) 전송
                let mtu = 1400;
                let rtp_header_size = if has_extension {28} else {12}; // Extension 포함시 28바이트
                let max_payload_bytes = mtu - rtp_header_size;
                let total_bytes = payload.len();
                let mut offset = 0;
                while offset < total_bytes {
                    let end = usize::min(offset + max_payload_bytes, total_bytes);
                    let chunk = &payload[offset..end];

                    let hdr = build_rtp_header(seq, timestamp, RTP_SSRC, has_extension);
                    let mut packet = Vec::with_capacity(rtp_header_size + chunk.len());
                    packet.extend_from_slice(&hdr);
                    
                    // Extension 헤더 추가 (첫 번째 청크에만)
                    if has_extension && offset == 0 {
                        let extension = RtpExtension::new_time_sync();
                        packet.extend_from_slice(&extension.to_bytes());
                    }
                    
                    packet.extend_from_slice(chunk);

                    // RTP 송출 주소의 포트를 항상 5004로 강제 지정
                    let mut rtp_addr = client_addr;
                    rtp_addr.set_port(5004);
                    match socket.send_to(&packet, &rtp_addr).await {
                        Ok(_) => sent_packets += 1,
                        Err(e) => {
                            send_errors += 1;
                            tracing::warn!("RTP send error to {}: {}", rtp_addr, e);
                        }
                    }
                    seq = seq.wrapping_add(1);
                    offset = end;
                }
                timestamp = timestamp.wrapping_add(frames as u32);
                extension_counter += 1; // Extension 카운터 증가
                
                if sent_packets % 100 == 0 {
                    tracing::info!("RTP stats for {}: sent={}, errors={}, channels={}, extensions={}", 
                                   client_addr, sent_packets, send_errors, channels, extension_counter / EXTENSION_INTERVAL);
                }
            }
        }
    }
    
    tracing::info!("RTP task for {} ended. Total sent={}, errors={}, channels={}", 
                   client_addr, sent_packets, send_errors, channels);
    
    // Remove client from state to allow reconnect
    let mut clients = clients.lock().unwrap();
    clients.remove(&client_addr);
} 