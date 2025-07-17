use rusqlite::{Connection, Result as SqliteResult, ToSql};
use tokio_rusqlite::Connection as AsyncConnection;
use crate::config::{Settings, ChannelConfig, ClientConfig};
use std::path::Path;
use tracing;

const DB_PATH: &str = "m32_server.db";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChannelMapping {
    pub physical_channel: u8,  // M32 물리적 채널 (1-32)
    pub logical_channel: u8,   // 논리적 채널 (1-32)
    pub name: String,          // 매핑된 채널 이름
    pub enabled: bool,         // 활성화 여부
    pub order_index: u8,       // 정렬 순서
}

pub struct Database {
    conn: AsyncConnection,
}

impl Database {
    pub async fn new() -> tokio_rusqlite::Result<Self> {
        let conn = AsyncConnection::open(DB_PATH).await?;
        let db = Database { conn };
        db.init_tables().await?;
        Ok(db)
    }

    async fn init_tables(&self) -> tokio_rusqlite::Result<()> {
        self.conn
            .call(|conn| {
                Ok(conn.execute(
                    "CREATE TABLE IF NOT EXISTS audio_config (
                        id INTEGER PRIMARY KEY,
                        sample_rate INTEGER NOT NULL,
                        compression TEXT NOT NULL,
                        jitter_interpolate_threshold INTEGER DEFAULT 100,
                        jitter_drop_threshold INTEGER DEFAULT 200,
                        active BOOLEAN DEFAULT 1,
                        created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
                    )",
                    [],
                )?)
            })
            .await?;

        self.conn
            .call(|conn| {
                Ok(conn.execute(
                    "CREATE TABLE IF NOT EXISTS channel_config (
                        id INTEGER PRIMARY KEY,
                        channel_id INTEGER NOT NULL,
                        name TEXT NOT NULL,
                        enabled BOOLEAN DEFAULT 1,
                        order_index INTEGER DEFAULT 0,
                        created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
                    )",
                    [],
                )?)
            })
            .await?;

        self.conn
            .call(|conn| {
                Ok(conn.execute(
                    "CREATE TABLE IF NOT EXISTS channel_mapping (
                        id INTEGER PRIMARY KEY,
                        physical_channel INTEGER NOT NULL,
                        logical_channel INTEGER NOT NULL,
                        name TEXT NOT NULL,
                        enabled BOOLEAN DEFAULT 1,
                        order_index INTEGER DEFAULT 0,
                        created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                        UNIQUE(physical_channel)
                    )",
                    [],
                )?)
            })
            .await?;

        self.conn
            .call(|conn| {
                Ok(conn.execute(
                    "CREATE TABLE IF NOT EXISTS client_config (
                        id INTEGER PRIMARY KEY,
                        client_ip TEXT NOT NULL,
                        client_port INTEGER NOT NULL,
                        sample_rate INTEGER,
                        compression TEXT,
                        config_json TEXT,
                        created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                        updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                        UNIQUE(client_ip, client_port)
                    )",
                    [],
                )?)
            })
            .await?;

        self.conn
            .call(|conn| {
                Ok(conn.execute(
                    "CREATE TABLE IF NOT EXISTS qos_metrics (
                        id INTEGER PRIMARY KEY,
                        client_ip TEXT NOT NULL,
                        client_port INTEGER NOT NULL,
                        packets_received INTEGER,
                        packets_expected INTEGER,
                        packet_loss_rate REAL,
                        jitter_ms REAL,
                        latency_ms REAL,
                        bandwidth_mbps REAL,
                        timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
                    )",
                    [],
                )?)
            })
            .await?;

        self.conn
            .call(|conn| {
                Ok(conn.execute(
                    "INSERT OR IGNORE INTO audio_config (id, sample_rate, compression, jitter_interpolate_threshold, jitter_drop_threshold) 
                     VALUES (1, 48000, 'none', 100, 200)",
                    [],
                )?)
            })
            .await?;

        for i in 1..=32 {
            self.conn
                .call(move |conn| {
                    let name = format!("CH{:02}", i);
                    Ok(conn.execute(
                        "INSERT OR IGNORE INTO channel_config (channel_id, name, order_index) 
                         VALUES (?, ?, ?)",
                        [&i as &dyn ToSql, &(name.as_str()) as &dyn ToSql, &i as &dyn ToSql],
                    )?)
                })
                .await?;
        }

        for i in 1..=32 {
            self.conn
                .call(move |conn| {
                    let name = format!("CH{:02}", i);
                    Ok(conn.execute(
                        "INSERT OR IGNORE INTO channel_mapping 
                         (physical_channel, logical_channel, name, order_index) 
                         VALUES (?, ?, ?, ?)",
                        [&i as &dyn ToSql, &i as &dyn ToSql, &(name.as_str()) as &dyn ToSql, &i as &dyn ToSql],
                    )?)
                })
                .await?;
        }

        tracing::info!("Database tables initialized");
        Ok(())
    }

    // 오디오 설정 관리
    pub async fn get_audio_config(&self) -> tokio_rusqlite::Result<(u32, String)> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT sample_rate, compression FROM audio_config WHERE active = 1 LIMIT 1"
                )?;
                let mut rows = stmt.query([])?;
                if let Some(row) = rows.next()? {
                    Ok((row.get(0)?, row.get(1)?))
                } else {
                    Ok((48000, "none".to_string()))
                }
            })
            .await
    }

    pub async fn update_audio_config(&self, sample_rate: u32, compression: &str) -> tokio_rusqlite::Result<()> {
        let sample_rate = sample_rate;
        let compression = compression.to_string();
        self.conn
            .call(move |conn| {
                conn.execute("UPDATE audio_config SET active = 0", [])?;
                conn.execute(
                    "INSERT INTO audio_config (sample_rate, compression, active) VALUES (?, ?, 1)",
                    [
                        &sample_rate as &dyn ToSql,
                        &(compression.as_str()) as &dyn ToSql,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    // 채널 설정 관리
    pub async fn get_channel_configs(&self) -> tokio_rusqlite::Result<Vec<ChannelConfig>> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT channel_id, name FROM channel_config WHERE enabled = 1 ORDER BY order_index"
                )?;
                let mut rows = stmt.query([])?;
                let mut channels = Vec::new();
                while let Some(row) = rows.next()? {
                    channels.push(ChannelConfig {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    });
                }
                Ok(channels)
            })
            .await
    }

    pub async fn update_channel_config(&self, channel_id: u8, name: &str, enabled: bool) -> tokio_rusqlite::Result<()> {
        let channel_id = channel_id;
        let name = name.to_string();
        let enabled = if enabled { 1 } else { 0 };
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE channel_config SET name = ?, enabled = ?, updated_at = CURRENT_TIMESTAMP \
                     WHERE channel_id = ?",
                    [
                        &(name.as_str()) as &dyn ToSql,
                        &enabled as &dyn ToSql,
                        &channel_id as &dyn ToSql,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    // 채널 매핑 관리
    pub async fn get_channel_mappings(&self) -> tokio_rusqlite::Result<Vec<ChannelMapping>> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT physical_channel, logical_channel, name, enabled, order_index 
                     FROM channel_mapping WHERE enabled = 1 ORDER BY order_index"
                )?;
                let mut rows = stmt.query([])?;
                let mut mappings = Vec::new();
                while let Some(row) = rows.next()? {
                    mappings.push(ChannelMapping {
                        physical_channel: row.get(0)?,
                        logical_channel: row.get(1)?,
                        name: row.get(2)?,
                        enabled: row.get(3)?,
                        order_index: row.get(4)?,
                    });
                }
                Ok(mappings)
            })
            .await
    }

    pub async fn get_channel_mapping(&self, physical_channel: u8) -> tokio_rusqlite::Result<Option<ChannelMapping>> {
        let physical_channel = physical_channel;
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT physical_channel, logical_channel, name, enabled, order_index \
                     FROM channel_mapping WHERE physical_channel = ?"
                )?;
                let mut rows = stmt.query([&physical_channel as &dyn ToSql])?;
                if let Some(row) = rows.next()? {
                    Ok(Some(ChannelMapping {
                        physical_channel: row.get(0)?,
                        logical_channel: row.get(1)?,
                        name: row.get(2)?,
                        enabled: row.get::<_, i32>(3)? != 0,
                        order_index: row.get(4)?,
                    }))
                } else {
                    Ok(None)
                }
            })
            .await
    }

    pub async fn update_channel_mapping(&self, mapping: &ChannelMapping) -> tokio_rusqlite::Result<()> {
        let name = mapping.name.clone();
        let physical_channel = mapping.physical_channel;
        let logical_channel = mapping.logical_channel;
        let enabled = mapping.enabled;
        let order_index = mapping.order_index;
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO channel_mapping \
                     (physical_channel, logical_channel, name, enabled, order_index, updated_at) \
                     VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
                    [
                        &physical_channel as &dyn ToSql,
                        &logical_channel as &dyn ToSql,
                        &(name.as_str()) as &dyn ToSql,
                        &enabled as &dyn ToSql,
                        &order_index as &dyn ToSql,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn delete_channel_mapping(&self, physical_channel: u8) -> tokio_rusqlite::Result<()> {
        let physical_channel = physical_channel;
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM channel_mapping WHERE physical_channel = ?",
                    [&physical_channel as &dyn ToSql],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn reorder_channel_mappings(&self, mappings: &[ChannelMapping]) -> tokio_rusqlite::Result<()> {
        let mappings = mappings.to_vec();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                for (index, mapping) in mappings.iter().enumerate() {
                    tx.execute(
                        "UPDATE channel_mapping SET order_index = ?, updated_at = CURRENT_TIMESTAMP \
                         WHERE physical_channel = ?",
                        [
                            &(index as u8 + 1) as &dyn ToSql,
                            &mapping.physical_channel as &dyn ToSql,
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    // 물리적 채널을 논리적 채널로 변환
    pub async fn get_logical_channel(&self, physical_channel: u8) -> tokio_rusqlite::Result<Option<u8>> {
        if let Some(mapping) = self.get_channel_mapping(physical_channel).await? {
            if mapping.enabled {
                Ok(Some(mapping.logical_channel))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    // 논리적 채널을 물리적 채널로 변환
    pub async fn get_physical_channel(&self, logical_channel: u8) -> tokio_rusqlite::Result<Option<u8>> {
        let logical_channel = logical_channel;
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT physical_channel FROM channel_mapping 
                     WHERE logical_channel = ? AND enabled = 1"
                )?;
                let mut rows = stmt.query([&logical_channel as &dyn ToSql])?;
                if let Some(row) = rows.next()? {
                    Ok(Some(row.get(0)?))
                } else {
                    Ok(None)
                }
            })
            .await
    }

    // 활성화된 채널 매핑만 가져오기
    pub async fn get_active_channel_mappings(&self) -> tokio_rusqlite::Result<Vec<ChannelMapping>> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT physical_channel, logical_channel, name, enabled, order_index 
                     FROM channel_mapping WHERE enabled = 1 ORDER BY order_index"
                )?;
                let mut rows = stmt.query([])?;
                let mut mappings = Vec::new();
                while let Some(row) = rows.next()? {
                    mappings.push(ChannelMapping {
                        physical_channel: row.get(0)?,
                        logical_channel: row.get(1)?,
                        name: row.get(2)?,
                        enabled: row.get(3)?,
                        order_index: row.get(4)?,
                    });
                }
                Ok(mappings)
            })
            .await
    }

    // 클라이언트 설정 관리
    pub async fn save_client_config(&self, client_ip: &str, client_port: u16, config: &ClientConfig) -> tokio_rusqlite::Result<()> {
        let config_json = serde_json::to_string(config).unwrap();
        let client_ip = client_ip.to_string();
        let compression = config.compression.as_deref().unwrap_or("none").to_string();
        let sample_rate = config.sample_rate.unwrap_or(48000);
        let client_port = client_port;
        let config_json = config_json;
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO client_config \
                     (client_ip, client_port, sample_rate, compression, config_json, updated_at) \
                     VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
                    [
                        &client_ip as &dyn ToSql,
                        &client_port as &dyn ToSql,
                        &sample_rate as &dyn ToSql,
                        &compression as &dyn ToSql,
                        &config_json as &dyn ToSql,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn get_client_config(&self, client_ip: &str, client_port: u16) -> tokio_rusqlite::Result<Option<ClientConfig>> {
        let client_ip = client_ip.to_string();
        let client_port = client_port;
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT config_json FROM client_config WHERE client_ip = ? AND client_port = ?"
                )?;
                let mut rows = stmt.query([
                    &client_ip as &dyn ToSql,
                    &client_port as &dyn ToSql,
                ])?;
                if let Some(row) = rows.next()? {
                    let config_json: String = row.get(0)?;
                    let config: ClientConfig = serde_json::from_str(&config_json).unwrap();
                    Ok(Some(config))
                } else {
                    Ok(None)
                }
            })
            .await
    }

    // QoS 메트릭 저장
    pub async fn save_qos_metrics(
        &self,
        client_ip: &str,
        client_port: u16,
        packets_received: u32,
        packets_expected: u32,
        packet_loss_rate: f64,
        jitter_ms: f64,
        latency_ms: f64,
        bandwidth_mbps: f64,
    ) -> tokio_rusqlite::Result<()> {
        let client_ip = client_ip.to_string();
        let client_port = client_port;
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO qos_metrics \
                     (client_ip, client_port, packets_received, packets_expected, \
                      packet_loss_rate, jitter_ms, latency_ms, bandwidth_mbps) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    [
                        &client_ip as &dyn ToSql,
                        &client_port as &dyn ToSql,
                        &packets_received as &dyn ToSql,
                        &packets_expected as &dyn ToSql,
                        &packet_loss_rate as &dyn ToSql,
                        &jitter_ms as &dyn ToSql,
                        &latency_ms as &dyn ToSql,
                        &bandwidth_mbps as &dyn ToSql,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    // 전체 설정을 Settings로 변환
    pub async fn get_settings(&self) -> tokio_rusqlite::Result<Settings> {
        let (sample_rate, compression) = self.get_audio_config().await?;
        let channels = self.get_channel_configs().await?;
        
        Ok(Settings {
            channels,
            compression,
            sample_rate,
        })
    }

    // 설정 변경 시 이벤트 로그
    pub async fn log_config_change(&self, change_type: &str, reason: &str, details: &str) -> tokio_rusqlite::Result<()> {
        let change_type = change_type.to_string();
        let reason = reason.to_string();
        let details = details.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS config_changes (
                        id INTEGER PRIMARY KEY,
                        change_type TEXT NOT NULL,
                        reason TEXT NOT NULL,
                        details TEXT,
                        timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
                    )",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO config_changes (change_type, reason, details) VALUES (?, ?, ?)",
                    [
                        &change_type as &dyn ToSql,
                        &reason as &dyn ToSql,
                        &details as &dyn ToSql,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    // 지터 임계값 조회
    pub async fn get_jitter_thresholds(&self) -> tokio_rusqlite::Result<(u32, u32)> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT jitter_interpolate_threshold, jitter_drop_threshold FROM audio_config WHERE active = 1 LIMIT 1"
                )?;
                let mut rows = stmt.query([])?;
                if let Some(row) = rows.next()? {
                    Ok((row.get(0)?, row.get(1)?))
                } else {
                    Ok((100, 200))
                }
            })
            .await
    }
    // 지터 임계값 갱신
    pub async fn update_jitter_thresholds(&self, interpolate: u32, drop: u32) -> tokio_rusqlite::Result<()> {
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE audio_config SET jitter_interpolate_threshold = ?, jitter_drop_threshold = ? WHERE active = 1",
                    [&interpolate as &dyn ToSql, &drop as &dyn ToSql],
                )?;
                Ok(())
            })
            .await
    }
} 