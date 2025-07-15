use crate::audio;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

const RTP_VERSION: u8 = 2;
const RTP_PAYLOAD_TYPE: u8 = 96; // dynamic
const RTP_SSRC: u32 = 0x12345678;
const RTP_PORT: u16 = 5004;
// RTP_ADDR는 사용하지 않음 (unicast)

fn build_rtp_header(seq: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    // Rust에서 C처럼 빠르게 하려면 unsafe 블록과 포인터 연산을 사용할 수 있습니다.
    // 아래는 C 스타일로 RTP 헤더를 빠르게 작성하는 예시입니다.
    let mut hdr = [0u8; 12];
    unsafe {
        let ptr = hdr.as_mut_ptr();
        *ptr.add(0) = (RTP_VERSION << 6) | 0; // V=2, P=0, X=0, CC=0
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
    hdr
}

pub async fn send_rtp_to_client(
    client_addr: SocketAddr,
    mut stop_rx: oneshot::Receiver<()>,
    clients: Arc<Mutex<HashMap<SocketAddr, (Instant, Option<oneshot::Sender<()>>)>>>,
) {
    let channels = 2;
    let frames = 160;
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
                let hdr = build_rtp_header(seq, timestamp, RTP_SSRC);
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
} 