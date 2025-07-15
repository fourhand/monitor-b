#[cfg(target_os = "linux")]
pub mod m32_audio {
    use alsa::card::Iter as CardIter;
    use alsa::card::Card;
    use alsa::ctl::Ctl;
    use alsa::pcm::{PCM, HwParams, Format, Access};
    use std::io;

    pub fn find_m32_usb_device() -> Option<String> {
        for card_index in CardIter::new().unwrap() {
            if let Ok(card) = Card::new(card_index) {
                let ctl = Ctl::new(&format!("hw:{}", card_index), false).ok()?;
                let card_info = ctl.card_info().ok()?;
                let name = card_info.get_name();
                let longname = card_info.get_longname();
                if name.contains("USB") || name.contains("M32") || longname.contains("USB") || longname.contains("M32") {
                    return Some(format!("hw:{},0", card_index));
                }
            }
        }
        None
    }

    pub fn open_and_capture(device: &str, channels: u32, rate: u32) -> io::Result<PCM> {
        let pcm = PCM::new(device, alsa::Direction::Capture, false)?;
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(channels)?;
        hwp.set_rate(rate, alsa::ValueOr::Nearest)?;
        hwp.set_format(Format::s32())?;
        hwp.set_access(Access::RWInterleaved)?;
        pcm.hw_params(&hwp)?;
        Ok(pcm)
    }

    // 실제 캡처 루프는 async로 래핑 필요
}

#[cfg(not(target_os = "linux"))]
pub mod m32_audio {
    // Mac/Windows 등에서는 mock 데이터 반환
    pub fn find_m32_usb_device() -> Option<String> {
        None
    }
    pub fn open_and_capture(_device: &str, _channels: u32, _rate: u32) -> Result<(), ()> {
        Err(())
    }
}

pub enum AudioMockMode {
    UnitTest,      // 모든 채널 동일 신호
    Auditory,      // 채널별로 다른 주파수(가청)
}

impl AudioMockMode {
    pub fn from_env() -> Self {
        match std::env::var("AUDIO_MOCK_MODE").as_deref() {
            Ok("unit") => AudioMockMode::UnitTest,
            Ok("auditory") => AudioMockMode::Auditory,
            _ => AudioMockMode::Auditory, // 기본값
        }
    }
}

/// 테스트/모의용: 모드에 따라 mock 오디오 프레임 생성
#[cfg(any(test, feature = "test-mock"))]
pub async fn capture_audio_frame_mock(channels: usize, frames: usize, mode: AudioMockMode) -> Vec<i32> {
    match mode {
        AudioMockMode::UnitTest => {
            let mut buffer = Vec::with_capacity(channels * frames);
            for f in 0..frames {
                let val = f as i32;
                for _ in 0..channels {
                    buffer.push(val);
                }
            }
            buffer
        }
        AudioMockMode::Auditory => {
            let base_freq = 500.0;
            let mut buffer = Vec::with_capacity(channels * frames);
            for f in 0..frames {
                for c in 0..channels {
                    let freq = base_freq + (c as f32) * 100.0;
                    let sample = ((f as f32 / frames as f32) * 2.0 * std::f32::consts::PI * freq).sin();
                    buffer.push((sample * 1000000.0) as i32);
                }
            }
            buffer
        }
    }
}

// 공통 인터페이스: 캡처 결과를 Vec<i32>로 반환 (mock 포함)
pub async fn capture_audio_frame(channels: usize, frames: usize) -> Vec<i32> {
    // 테스트 환경에서는 ALSA/OnceLock을 건너뜀
    if std::env::var("AUDIO_MOCK_MODE").is_ok() {
        let mode = AudioMockMode::from_env();
        return match mode {
            AudioMockMode::UnitTest => {
                let mut buffer = Vec::with_capacity(channels * frames);
                for _f in 0..frames {
                    for c in 0..channels {
                        buffer.push(c as i32);
                    }
                }
                buffer
            }
            AudioMockMode::Auditory => {
                let mut buffer = Vec::with_capacity(channels * frames);
                for _f in 0..frames {
                    for c in 0..channels {
                        buffer.push(c as i32);
                    }
                }
                buffer
            }
        };
    }
    #[cfg(target_os = "linux")]
    {
        use alsa::pcm::PCM;
        use std::sync::OnceLock;
        static PCM_HANDLE: OnceLock<Option<PCM>> = OnceLock::new();
        let pcm = PCM_HANDLE.get_or_init(|| {
            let dev = m32_audio::find_m32_usb_device();
            dev.and_then(|d| m32_audio::open_and_capture(&d, channels as u32, 48000).ok())
        });
        if let Some(pcm) = pcm {
            let io = pcm.io_i32().ok();
            let mut buffer = vec![0i32; channels * frames];
            if let Some(io) = io {
                let _ = io.readi(&mut buffer);
                return buffer;
            }
        }
        // fallback: mock
    }
    // fallback: mock (리눅스에서 ALSA 실패시)
    let mut buffer = Vec::with_capacity(channels * frames);
    for _f in 0..frames {
        for c in 0..channels {
            buffer.push(c as i32);
        }
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_capture_audio_frame_unit() {
        let channels = 4;
        let frames = 8;
        let data = capture_audio_frame_mock(channels, frames, AudioMockMode::UnitTest).await;
        assert_eq!(data.len(), channels * frames);
        for f in 0..frames {
            let start = f * channels;
            let end = start + channels;
            let frame = &data[start..end];
            for &v in frame {
                assert_eq!(v, f as i32);
            }
        }
    }

    #[tokio::test]
    async fn test_capture_audio_frame_auditory() {
        let channels = 4;
        let frames = 8;
        let data = capture_audio_frame_mock(channels, frames, AudioMockMode::Auditory).await;
        assert_eq!(data.len(), channels * frames);
        let frame0 = &data[0..channels];
        let mut unique = std::collections::HashSet::new();
        for &v in frame0 {
            unique.insert(v);
        }
        assert!(unique.len() > 1, "채널별로 값이 달라야 함");
    }
} 