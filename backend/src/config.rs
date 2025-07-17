use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    pub channels: Vec<ChannelConfig>,
    pub compression: String,   // "none", "opus" 등
    pub sample_rate: u32,      // Hz
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChannelConfig {
    pub id: u8,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClientConfig {
    pub channels: Option<Vec<ChannelConfig>>,
    pub sample_rate: Option<u32>,
    pub compression: Option<String>,
}

const CONFIG_PATH: &str = "config.yaml";

pub async fn load_config() -> Settings {
    if Path::new(CONFIG_PATH).exists() {
        let data = fs::read_to_string(CONFIG_PATH).expect("config read fail");
        serde_yaml::from_str(&data).expect("config parse fail")
    } else {
        // 기본값 생성
        let default = Settings {
            channels: (1..=32).map(|i| ChannelConfig { id: i, name: format!("CH{:02}", i) }).collect(),
            compression: "none".to_string(),
            sample_rate: 48000,
        };
        save_config(&default).await;
        default
    }
}

pub async fn save_config(cfg: &Settings) {
    let data = serde_yaml::to_string(cfg).expect("config serialize fail");
    fs::write(CONFIG_PATH, data).expect("config write fail");
} 