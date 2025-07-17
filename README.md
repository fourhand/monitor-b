
# M32 USB 오디오 송출 시스템 — 최신 요구사항 스펙

## 개요

Midas M32의 USB 멀티채널 오디오를 캡처해 Wi-Fi를 통해 클라이언트(안드로이드 + iOS)로 저지연 송출한다.  
클라이언트는 서버가 매핑한 채널만 수신하여 로컬에서 믹싱하고, 개인 설정을 저장하며, 최신 패킷만 재생한다.

## 시스템 구성

### 서버

- 리눅스 서버 (Rust 백엔드 + FastAPI 웹 UI)
- M32 USB 연결 (32채널 오디오 캡처)
- UDP/RTP로 매핑된 채널 송출
- 설정은 SQLite로 관리
- 이벤트 및 클라이언트 상태 통신은 WebSocket 사용
- 웹 UI에서 채널 이름, 오디오 포맷, 송출 매핑 설정 가능 (비밀번호 보호)
  - DAW로부터 수신한 32채널 중 송출 여부와 순서 지정
  - 채널 이름 기본값은 `CH01`, `CH02` …
- 클라이언트에서 설정 변경 불가
- 송출 중 설정 변경 시 서버는 세션을 종료하고 클라이언트에 알림
- 에러 로깅
- 네트워크 품질 모니터링 (손실률, 지터, 지연)

### 클라이언트

- 안드로이드 + iOS 앱
- UDP/RTP로 서버가 매핑한 채널 수신
- WebSocket으로 상태, 설정, 이벤트 송수신
- 로컬 믹싱 및 재생
- 로컬에 개인 설정 저장
  - 채널별 이름, 색상, 이미지, 볼륨, 팬, EQ, 뮤트, 샘플레이트, 압축 모드
- 최신 패킷만 재생
- 서버의 샘플레이트/코덱 변경 시 자동 재연결

## 네트워크

- 전용 5GHz AP
- 유니캐스트 UDP + WebSocket
- 클라이언트 최대 20명
- NTP 동기화
- UDP DSCP (QoS) 설정 가능 (예: EF 46)

## 오디오 데이터

| 항목 | 내용 |
|------|------|
| 프로토콜 | UDP + RTP |
| 방향 | 서버 → 클라이언트 |
| 포트 | 고정 UDP 포트 (예: 5004) |
| 데이터 | 매핑된 채널 오디오 데이터 |
| 송출 조건 | 클라이언트가 WebSocket으로 client-config 전달 후 |
| 클라이언트 heartbeat 유지 |
| 서버는 30초 이상 heartbeat 없으면 송출 중단 |

## 메타데이터

| 항목 | 내용 |
|------|------|
| 프로토콜 | HTTP (REST) |
| 방향 | 클라이언트 → 서버 |
| 데이터 | 채널 이름/번호/속성, 서버 상태, 네트워크 품질 |
| `/audio-meta` | 현재 샘플레이트, 채널 정보 확인 |

## 서버 탐색 방법

- mDNS: `m32-server.local` 탐색
- QR 코드: 웹 UI에서 표시된 QR코드 스캔
- 수동 입력: 클라이언트에서 직접 IP 입력

## 클라이언트 ↔ 서버 연결 흐름

1. 클라이언트가 mDNS, QR 코드, 수동 입력으로 서버 IP 탐색
2. WebSocket을 열어 상태 통신 시작
3. `/request-audio?udp_port=포트` 요청
4. `client-config`를 WebSocket으로 전송
5. 서버가 UDP 송출 시작
6. 클라이언트가 주기적으로 heartbeat 전송
7. 서버가 이벤트 발생 시 WebSocket으로 알림
8. 클라이언트가 `/audio-meta` 호출 후 재연결

## 클라이언트 채널별 설정

| 속성 | 타입 | 설명 |
|------|------|------|
| id | int | USB 채널 번호 |
| name | string | 채널 이름 |
| mute | bool | 음소거 |
| hidden | bool | UI 숨김 |
| order | int | UI 순서 |
| volume | float | 볼륨 |
| solo | bool | 솔로 |
| pan | float | 좌/우 밸런스 |
| eq | object | EQ |
| limit | object/bool | 리미터 |

### EQ 예시

```json
"eq": {
  "low": -3.0,
  "mid": 0.0,
  "high": +2.5
}
```

## WebSocket 메시지 예시

### 클라이언트 → 서버

#### heartbeat

```json
{
  "type": "heartbeat",
  "timestamp": "2025-07-15T10:05:00Z"
}
```

#### client-config

```json
{
  "type": "client_config",
  "payload": {
    "channels": [
      { "id": 1, "name": "Vocal", "mute": false }
    ],
    "sample_rate": 44100,
    "compression": "opus"
  }
}
```

### 서버 → 클라이언트

#### 이벤트 메시지

```json
{
  "type": "event",
  "payload": {
    "type": "config_changed",
    "reason": "latency too high, lowering sample rate",
    "new_sample_rate": 32000
  }
}
```

#### 기타 이벤트

| type | 설명 |
|------|------|
| reconnect | 서버 재시작 필요 |
| config_changed | 설정 변경됨 |
| server_shutdown | 서버 종료 예정 |
| network_warning | 손실/지연/지터 경고 |
| custom_message | 사용자 정의 메시지 |

## 동적 샘플레이트 변경

- 서버는 latency를 모니터링하여 임계치를 초과하면 샘플레이트를 낮춘다.
- 클라이언트에 `config_changed` 이벤트를 WebSocket으로 전송.
- 클라이언트는 `/audio-meta`를 호출해 새 샘플레이트를 확인하고 재연결.
- 새 샘플레이트는 SQLite에 기록.

### 샘플레이트 단계 예시

| 단계 | 샘플레이트 (Hz) |
|------|----------------|
| 기본 | 48000 |
| 단계 1 | 44100 |
| 단계 2 | 32000 |
| 단계 3 | 24000 |

### SQLite 예시

```sql
UPDATE audio_config
SET sample_rate = 32000
WHERE active = 1;
```

## 오디오 데이터 처리

- 클라이언트 연결 시에만 오디오 데이터 생성 후 송출.
- 클라이언트가 모두 끊기면 데이터 생성/송출 중단.
- 버퍼나 저장 없이 실시간 송출.
- 테스트 모드에서는 각 채널의 값이 채널 번호와 일치하도록 송출하여 검증 가능.

## 요약

- 음원 데이터: UDP + RTP 송출
- 상태/이벤트: WebSocket
- 설정: SQLite
- 네트워크 품질에 따라 서버가 샘플레이트를 낮출 수 있으며, 클라이언트는 재연결
- 클라이언트는 최신 패킷만 재생하고 로컬 설정으로 믹싱
- QoS(Differentiated Services) 설정 가능

## 🧪 QoS 테스트 (다수 클라이언트)

### 다수 테스트 클라이언트 실행

- **동시 접속 테스트**: 여러 개의 test-client를 동시에 실행하여 서버의 다중 클라이언트 처리 능력을 테스트
- **네트워크 품질 모니터링**: 각 클라이언트별 패킷 손실률, 지터, 지연 시간 측정
- **부하 테스트**: 클라이언트 수를 점진적으로 증가시켜 서버 성능 한계 확인

### 테스트 클라이언트 실행 방법

#### 1. 단일 클라이언트 (기본)
```bash
cd test-client
cargo run
```

#### 2. 다수 클라이언트 (QoS 테스트)
```bash
# 터미널 1
cd test-client
cargo run

# 터미널 2 (다른 UDP 포트 사용)
cd test-client
UDP_PORT=5005 cargo run

# 터미널 3 (또 다른 UDP 포트)
cd test-client
UDP_PORT=5006 cargo run
```

#### 3. 자동화된 다수 클라이언트 실행 스크립트
```bash
#!/bin/bash
# test-multiple-clients.sh
for i in {1..5}; do
    UDP_PORT=$((5004 + i)) cargo run &
    echo "Started client $i on port $((5004 + i))"
    sleep 2
done
wait
```

### 테스트 클라이언트 설정

#### 환경변수 설정
테스트 클라이언트는 환경변수를 통해 다양한 설정을 조정할 수 있습니다:

```bash
# UDP 포트 설정 (기본값: 5004)
export UDP_PORT=5005

# 강제 지연 설정 (밀리초, 기본값: 0)
export FORCE_DELAY_MS=100

# 클라이언트 실행
cargo run
```

#### 강제 지연 기능
- `FORCE_DELAY_MS`: RTP 패킷 수신 후 처리 전에 강제로 지연을 추가
- 네트워크 지연 시뮬레이션에 사용
- QoS 테스트 시 다양한 지연 조건에서의 동작 확인 가능

#### 사용 예시
```bash
# 지연 없는 기본 실행
cargo run

# 50ms 지연으로 실행
FORCE_DELAY_MS=50 cargo run

# 다른 포트 + 100ms 지연으로 실행
UDP_PORT=5005 FORCE_DELAY_MS=100 cargo run

# 다수 클라이언트 + 다양한 지연으로 테스트
FORCE_DELAY_MS=0 cargo run &    # 클라이언트 1: 지연 없음
FORCE_DELAY_MS=50 cargo run &   # 클라이언트 2: 50ms 지연
FORCE_DELAY_MS=100 cargo run &  # 클라이언트 3: 100ms 지연
```

#### 지연 시뮬레이션 시나리오
- **정상 네트워크**: `FORCE_DELAY_MS=0` (기본값)
- **약간의 지연**: `FORCE_DELAY_MS=10-50`
- **중간 지연**: `FORCE_DELAY_MS=50-100`
- **높은 지연**: `FORCE_DELAY_MS=100-500`
- **매우 높은 지연**: `FORCE_DELAY_MS=500+`

### QoS 테스트 항목

| 테스트 항목 | 설명 | 측정 방법 |
|------------|------|-----------|
| **동시 접속 수** | 서버가 처리할 수 있는 최대 클라이언트 수 | 클라이언트 수를 점진적으로 증가 |
| **패킷 손실률** | RTP 패킷 전송 중 손실된 패킷 비율 | 클라이언트에서 수신 패킷 수 vs 예상 패킷 수 |
| **지터 (Jitter)** | 패킷 간 도착 시간 변동 | RTP 타임스탬프 기반 계산 |
| **지연 시간** | 서버에서 클라이언트까지의 전송 지연 | RTP 시퀀스 번호와 타임스탬프 분석 |
| **대역폭 사용량** | 네트워크 대역폭 소모량 | 네트워크 모니터링 도구 사용 |
| **CPU 사용률** | 서버 CPU 사용률 | 시스템 모니터링 도구 사용 |

### 테스트 시나리오

#### 시나리오 1: 점진적 부하 테스트
1. 1개 클라이언트로 시작
2. 5분마다 1개씩 클라이언트 추가 (최대 20개)
3. 각 단계에서 10분간 안정성 테스트
4. 패킷 손실률, 지터, 지연 시간 기록

#### 시나리오 2: 스트레스 테스트
1. 20개 클라이언트 동시 접속
2. 30분간 연속 송출
3. 네트워크 품질 지속적 모니터링
4. 서버 리소스 사용률 관찰

#### 시나리오 3: 네트워크 불안정 시뮬레이션
1. 네트워크 지연/손실 시뮬레이션 도구 사용
2. 다양한 네트워크 조건에서 테스트
3. 클라이언트 재연결 능력 확인

### 모니터링 및 로깅

#### 서버 측 모니터링
```bash
# 서버 로그에서 클라이언트별 통계 확인
tail -f backend.log | grep "RTP stats"

# 시스템 리소스 모니터링
htop
iotop
```

#### 클라이언트 측 모니터링
- 각 클라이언트는 수신한 RTP 패킷 수, 손실률, 지터 정보를 로그로 출력
- 주기적으로 QoS 메트릭을 서버에 전송 (`POST /qos-metrics`)

#### 네트워크 모니터링
```bash
# 네트워크 트래픽 모니터링
iftop -i eth0
nethogs

# 패킷 캡처 및 분석
tcpdump -i eth0 -w capture.pcap
```

### QoS 메트릭 수집 API

#### 클라이언트 → 서버 QoS 데이터 전송
```json
POST /qos-metrics
{
  "client_id": "client_001",
  "timestamp": "2024-01-01T12:00:00Z",
  "metrics": {
    "packets_received": 1500,
    "packets_expected": 1500,
    "packet_loss_rate": 0.0,
    "jitter_ms": 2.5,
    "latency_ms": 15.2,
    "bandwidth_mbps": 2.1
  }
}
```

#### 서버 응답
```json
{
  "status": "ok",
  "server_metrics": {
    "total_clients": 5,
    "total_packets_sent": 7500,
    "server_cpu_usage": 45.2,
    "server_memory_usage": 128.5
  }
}
```

### 성능 목표

| 메트릭 | 목표값 | 측정 조건 |
|--------|--------|-----------|
| 최대 동시 클라이언트 | 20개 | 안정적 송출 |
| 패킷 손실률 | < 1% | 정상 네트워크 |
| 지터 | < 5ms | 정상 네트워크 |
| 지연 시간 | < 50ms | 정상 네트워크 |
| 서버 CPU 사용률 | < 80% | 20개 클라이언트 |

### 테스트 결과 분석

- **성능 그래프**: 클라이언트 수 vs 각종 메트릭 그래프 생성
- **한계점 식별**: 성능이 급격히 저하되는 지점 확인
- **최적화 방안**: 병목 지점 분석 및 개선 방안 도출

## 네트워크 품질 모니터링 및 시각 동기화 (NTP)

- 클라이언트는 서버와의 정확한 latency(지연) 측정을 위해 **WebSocket 기반 NTP 동기화**를 수행한다.
- 표준 NTP(UDP/123) 대신, WebSocket 메시지로 시각 동기화 및 왕복 지연(RTT) 보정이 가능하도록 설계한다.
- 동기화 과정에서 NTP 프로토콜의 핵심(T1~T4, RTT, offset) 계산을 그대로 적용한다.

### WebSocket 기반 NTP 동기화 메시지 예시

클라이언트 → 서버:
```json
{
  "type": "ntp_request",
  "t1": 1721123456789
}
```
서버 → 클라이언트:
```json
{
  "type": "ntp_response",
  "t1": 1721123456789,
  "t2": 1721123456790,
  "t3": 1721123456791
}
```
- t1: 클라이언트가 요청을 보낸 시각 (ms, UTC)
- t2: 서버가 요청을 수신한 시각 (ms, UTC)
- t3: 서버가 응답을 보낸 시각 (ms, UTC)
- t4: 클라이언트가 응답을 수신한 시각 (ms, UTC, 클라이언트에서 측정)

### NTP 오프셋/지연 계산 공식
```
round_trip_delay = (t4 - t1) - (t3 - t2)
offset = ((t2 - t1) + (t3 - t4)) / 2
```
- offset을 클라이언트 시각에 더하면 서버 시각과 동기화됨
- round_trip_delay(RTT)가 너무 크면(예: 100ms 이상) 동기화 무효 처리 가능

### RTP latency 측정 활용 예시
- 클라이언트는 NTP offset을 적용해 RTP 패킷 수신 시 latency(ms) 계산 가능
- 예: `latency_ms = (client_receive_time_ms + offset) - server_rtp_timestamp_ms`
- 정확한 latency, 지터, 패킷 손실률 등 QoS 메트릭 산출에 활용

### 장점
- 별도 UDP 포트/시스템 NTP 설정 없이, 애플리케이션 레벨에서 정확한 시각 동기화 가능
- WebSocket 연결만으로 실시간 품질 모니터링 및 자동화된 QoS 분석 가능

## 패킷 처리 및 오디오 품질 최적화

### 패킷 버림 vs 보간 전략

실시간 오디오 시스템에서 패킷 버림은 '지직' 노이즈를 발생시킬 수 있으므로 신중한 접근이 필요합니다.

#### 1. 보간(Interpolation) 기반 패킷 복구
- 이전 패킷과 다음 패킷 사이를 보간하여 누락된 오디오 데이터 복구
- 저주파 성분은 보간이 효과적, 고주파 성분은 보간 시 아티팩트 발생 가능
- 패킷 버림보다 자연스러운 오디오 재생 가능

#### 2. 적응형 버퍼링
- 지터 수준에 따라 동적으로 버퍼 크기를 조정
- 낮은 지터: 작은 버퍼로 낮은 지연 유지
- 높은 지터: 큰 버퍼로 안정성 확보
- 실시간 네트워크 상황에 적응

#### 3. 지터 임계값 기반 처리
- 지터 임계값은 시스템 환경에 따라 튜닝이 필요하며, SQLite에 필드로 저장해 API로 관리할 수 있습니다.
- 예시:
```rust
// 지터 임계값 (SQLite에서 관리)
const JITTER_INTERPOLATE_THRESHOLD: u32 = 100;  // 100ms: 보간 시작
const JITTER_DROP_THRESHOLD: u32 = 200;         // 200ms: 패킷 버림
```

### 오디오 주파수 분석 및 EQ 처리의 지연 영향

- 저주파/고주파 구분이나 EQ 처리를 위해 FFT(고속 푸리에 변환)를 사용할 수 있습니다.
- FFT는 실시간 오디오 처리에서 약간의 추가 지연(수백 마이크로초~1ms 내외)을 유발할 수 있습니다.
- EQ 역시 실시간 처리가 가능하지만, 복잡한 필터나 고차 FFT를 사용할 경우 누적 지연이 발생할 수 있으므로, 시스템의 실시간성 요구사항에 따라 적용 범위를 결정해야 합니다.
- 실시간성이 매우 중요한 경우, FFT/EQ 적용 구간을 최소화하거나, 저지연 최적화된 알고리즘을 사용하는 것이 권장됩니다.

## RTP 헤더 Extension을 활용한 시각 동기화

### RTP Extension 헤더 구조

실시간 오디오 스트리밍에서 정확한 latency 측정을 위해 RTP 헤더 Extension을 활용합니다.

#### Extension 헤더 구조 (RFC3550표준)
```rust
struct RtpExtension [object Object]
    profile: u16,        //0BEDE (표준 프로파일)
    length: u16,         // Extension 데이터 길이 (워드 단위)
    server_time_ms: u64, // 서버 UTC 시각 (ms)
    event_flags: u8    // 이벤트 플래그
    reserved:u8;3],   // 예약된 바이트
}
```

#### 주기적 Extension 전송
- **5초마다** RTP 패킷에 Extension 헤더 포함
- 일반 패킷은 Extension 없이 전송하여 오버헤드 최소화
- 클라이언트는 Extension 플래그(X=1시각 동기화 패킷 구분

#### RTP 패킷 구조
```기본 RTP 헤더 12이트] +Extension 헤더16 + [오디오 데이터]
```

### 클라이언트에서의 처리

#### Extension 헤더 파싱
```rust
// RTP 헤더에서 Extension 플래그 확인
if (rtp_header[0& 0x10) != 0 [object Object]    // Extension 헤더 파싱
    let server_time_ms = parse_extension_header(&rtp_packet[12..28]);
    let latency = (client_time_ms + ntp_offset) - server_time_ms;
    println!("[RTP] Extension: server_time=[object Object]ency=[object Object]s, server_time_ms, latency);
}
```

#### 장점
- **기존 RTP 표준 준수**: RFC3550 Extension 헤더 활용
- **주기적 전송**: 5마다만 Extension 포함하여 오버헤드 최소화
- **정확한 시각 동기화**: 서버 송신 시각을 직접 전달하여 latency 계산 정확도 향상
- **WebSocket 독립**: RTP 스트림 내에서 시각 동기화 처리

#### Extension 이벤트 플래그
| 플래그 값 | 의미 |
|-----------|------|
| 0x1 | time_sync (시각 동기화) |
| 0x2 | status_update (상태 업데이트) |
| 0x04 | control_message (제어 메시지) |

### 구현 예시

#### 서버 측 (RTP 송신)
```rust
let mut extension_counter =0extension_interval =250 // 5 = 250개 패킷 (20ms 간격)

if extension_counter % extension_interval == 0 [object Object]    // Extension 헤더 포함
    let extension = RtpExtension [object Object]
        profile: 0xBEDE,
        length: 323       server_time_ms: chrono::Utc::now().timestamp_millis() as u64,
        event_flags: 001, // time_sync 플래그
        reserved: [0;3],
    };
    // RTP 패킷에 Extension 추가
} else [object Object]    // 일반 RTP 패킷 (Extension 없음)
}
```

#### 클라이언트 측 (RTP 수신)
```rust
// RTP 헤더 파싱
let has_extension = (rtp_header[0 & 0x10

if has_extension [object Object]    // Extension 헤더 파싱 (12-28바이트)
    let server_time_ms = u64::from_be_bytes([
        rtp_packet[12], rtp_packet[13], rtp_packet[14, rtp_packet[15        rtp_packet[16], rtp_packet[17], rtp_packet[18], rtp_packet[19]
    ]);
    
    let client_time_ms = chrono::Utc::now().timestamp_millis() as u64;
    let (ntp_offset, _) = *ntp_state.lock().unwrap();
    let latency = (client_time_ms + ntp_offset) - server_time_ms;
    
    println!("[RTP] Extension sync: server={}, client=[object Object], latency={}ms", 
             server_time_ms, client_time_ms, latency);
}
```

## 클라이언트의 지터 임계값 적용 정책

- **지터 임계값(Jitter Threshold)은 클라이언트에서 적용**하며, 서버는 기본값을 제공(API: `/jitter-thresholds`).
- 클라이언트는 다음 우선순위로 임계값을 결정:
    1. **환경변수**(`JITTER_INTERPOLATE_MS`, `JITTER_DROP_MS`)가 설정되어 있으면 이를 우선 사용
    2. 환경변수가 없으면 서버의 `/jitter-thresholds` API에서 기본값을 받아 사용
    3. 서버 API도 실패하면 코드 내 기본값(100ms/200ms)을 사용
- 이 구조로 "클라이언트가 직접 설정하지 않으면 서버의 기본값을 따른다"는 정책이 구현됨
- (EQ, FFT 등 오디오 품질 관련 설정도 클라이언트에서 개별적으로 적용 가능)

### 지터 임계값 REST API

#### GET /jitter-thresholds
- 서버의 지터 임계값(보간/버림 기준)을 조회
- 응답 예시:
```json
{
  "interpolate": 100,
  "drop": 200
}
```

#### POST /jitter-thresholds
- 서버의 지터 임계값(보간/버림 기준)을 갱신
- 요청 예시:
```json
{
  "interpolate": 120,
  "drop": 250
}
```
- 응답 예시:
```json
{
  "status": "ok"
}
```

#### 클라이언트의 지터 임계값 서버 수신 규격 및 동작 흐름

- 클라이언트는 실행 시 서버의 `/jitter-thresholds` REST API를 호출하여 지터 임계값(보간/버림 기준)을 받아옵니다.
- 서버 응답(JSON):
```json
{
  "interpolate": 100,
  "drop": 200
}
```
- 클라이언트는 환경변수(`JITTER_INTERPOLATE_MS`, `JITTER_DROP_MS`)가 없을 경우 이 값을 사용합니다.
- 예시 Rust 코드:
```