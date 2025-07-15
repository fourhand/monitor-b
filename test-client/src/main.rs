use reqwest::Client;
use std::net::UdpSocket;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use rustfft::{FftPlanner, num_complex::Complex};

const BACKEND_ADDR: &str = "127.0.0.1:8000";
const LOCAL_PORT: u16 = 5004;
const CHANNELS: usize = 32;
const FRAMES: usize = 256;

#[tokio::main]
async fn main() {
    // 1. 백엔드에 /request-audio 요청
    let client = Client::new();
    let url = format!("http://{}/request-audio", BACKEND_ADDR);
    let resp = client.get(&url).send().await;
    println!("/request-audio: {:?}", resp);

    // 2. UDP로 RTP 패킷 수신
    let socket = UdpSocket::bind(("0.0.0.0", LOCAL_PORT)).expect("UDP bind fail");
    socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    println!("Listening for RTP on udp:{}...", LOCAL_PORT);
    println!("Press 'q' to quit.");

    // 종료 플래그
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    // heartbeat task
    let heartbeat_running = running.clone();
    let heartbeat_client = client.clone();
    tokio::spawn(async move {
        let url = format!("http://{}/heartbeat", BACKEND_ADDR);
        while heartbeat_running.load(Ordering::SeqCst) {
            let _ = heartbeat_client.get(&url).send().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });

    // 키 입력 감지 스레드
    thread::spawn(move || {
        use std::io::{self, Read};
        let stdin = io::stdin();
        for b in stdin.bytes() {
            if let Ok(b'q') = b {
                running_clone.store(false, Ordering::SeqCst);
                break;
            }
        }
    });

    let mut buf = [0u8; 4096];
    let mut total_packets = 0;
    let mut did_fft = false;
    while running.load(Ordering::SeqCst) {
        match socket.recv(&mut buf) {
            Ok(n) if n > 12 => {
                total_packets += 1;
                // 3. RTP 헤더(12바이트) 이후 payload에서 32채널 x 256프레임 i32 추출
                let payload = &buf[12..n];
                let mut values = Vec::new();
                for f in 0..FRAMES {
                    let mut frame = Vec::new();
                    for c in 0..CHANNELS {
                        let idx = (f * CHANNELS + c) * 4;
                        if idx + 4 <= payload.len() {
                            let v = i32::from_le_bytes([
                                payload[idx],
                                payload[idx + 1],
                                payload[idx + 2],
                                payload[idx + 3],
                            ]);
                            frame.push(v);
                        }
                    }
                    values.push(frame);
                }
                // 채널별 값 검증: 각 채널 값이 채널 번호와 일치하는지 확인
                for (frame_idx, frame) in values.iter().enumerate() {
                    let mut ok = true;
                    for (c, &v) in frame.iter().enumerate() {
                        if v != c as i32 {
                            ok = false;
                            println!("[CHECK] Frame {}: Channel {} value {} != expected {}", frame_idx, c, v, c);
                        }
                    }
                    if ok {
                        println!("[CHECK] Frame {}: All channel values correct", frame_idx);
                    }
                }
                // FFT 분석: 첫 번째 프레임, 모든 채널 (1회만)
                if !did_fft {
                    println!("Received {} frames, {} channels per frame", values.len(), if !values.is_empty() { values[0].len() } else { 0 });
                    if !values.is_empty() && values[0].len() == CHANNELS {
                        for c in 0..CHANNELS {
                            let channel_samples: Vec<f32> = values.iter().filter_map(|frame| frame.get(c)).map(|&v| v as f32).collect();
                            if channel_samples.len() == values.len() {
                                let len = channel_samples.len();
                                let mut planner = FftPlanner::new();
                                let fft = planner.plan_fft_forward(len);
                                let mut buffer: Vec<Complex<f32>> = channel_samples.into_iter().map(|v| Complex{ re: v, im: 0.0 }).collect();
                                fft.process(&mut buffer);
                                let (max_idx, max_val) = buffer.iter().take(len/2).enumerate()
                                    .map(|(i, c)| (i, c.norm()))
                                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                                    .unwrap();
                                let sample_rate = 48000.0;
                                let freq = max_idx as f32 * sample_rate / len as f32;
                                println!("[FFT] Channel {}: Dominant frequency: {:.2} Hz (bin {} magnitude {:.2})", c, freq, max_idx, max_val);
                            }
                        }
                        did_fft = true;
                    } else {
                        println!("Not enough data for FFT: frames={}, channels per frame={}", values.len(), if !values.is_empty() { values[0].len() } else { 0 });
                    }
                }
            }
            _ => {}
        }
    }
    println!("Total RTP packets received: {}", total_packets);
}
