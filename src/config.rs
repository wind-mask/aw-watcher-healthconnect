use std::{net::IpAddr, time::Duration};

use clap::{Parser, ValueEnum};

use crate::WATCHER_NAME;

#[derive(Clone, Copy, ValueEnum)]
pub enum Protocol {
    Http,
    Https,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

#[derive(Parser)]
#[command(name = WATCHER_NAME, version, about = "接收 HC Webhook JSON 并写入 ActivityWatch")]
pub struct Config {
    /// Webhook 监听地址；局域网使用时设为 0.0.0.0 并配置 token
    #[arg(long, env = "AW_HEALTHCONNECT_HOST", default_value = "127.0.0.1")]
    pub host: IpAddr,

    #[arg(long, env = "AW_HEALTHCONNECT_PORT", default_value_t = 5700)]
    pub port: u16,

    #[arg(long, env = "AW_HOST", default_value = "127.0.0.1")]
    pub aw_host: String,

    /// 默认生产端口 5600；--testing 时默认 5666
    #[arg(long, env = "AW_PORT")]
    pub aw_port: Option<u16>,

    #[arg(long, env = "AW_PROTOCOL", value_enum, default_value = "http")]
    pub aw_protocol: Protocol,

    #[arg(long)]
    pub testing: bool,

    /// 稳定的来源设备标识；不同手机应使用不同标识
    #[arg(long, env = "AW_HEALTHCONNECT_DEVICE_ID", default_value = "android", value_parser = parse_device_id)]
    pub device_id: String,

    /// 接收端 X-API-Key；建议只通过环境变量提供，避免命令行泄露
    #[arg(long, env = "AW_HEALTHCONNECT_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    /// ActivityWatch 服务端认证，与 Webhook 接收端 token 无关
    #[arg(long, env = "AW_API_KEY", hide_env_values = true)]
    pub aw_api_key: Option<String>,

    /// 单次 Webhook 总响应超时（含读取请求体和等待其他请求）
    #[arg(long, env = "AW_HEALTHCONNECT_REQUEST_TIMEOUT", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=300))]
    pub request_timeout: u64,

    /// JSON 请求体上限，默认 16 MiB
    #[arg(long, env = "AW_HEALTHCONNECT_MAX_BODY_BYTES", default_value_t = 16 * 1024 * 1024)]
    pub max_body_bytes: usize,

    #[arg(long)]
    pub verbose: bool,
}

impl Config {
    pub fn server_port(&self) -> u16 {
        self.aw_port
            .unwrap_or(if self.testing { 5666 } else { 5600 })
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.max_body_bytes > 0, "max-body-bytes 必须大于零");
        for (name, secret) in [("token", &self.token), ("aw-api-key", &self.aw_api_key)] {
            if let Some(value) = secret {
                anyhow::ensure!(!value.trim().is_empty(), "{name} 不能是空字符串");
                anyhow::ensure!(
                    value.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                    "{name} 必须仅包含可用于 HTTP 头的非空 ASCII 字符"
                );
            }
        }
        Ok(())
    }
}

fn parse_device_id(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("设备标识需为 1–64 个 ASCII 字母、数字、连字符或下划线".into());
    }
    Ok(value.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn testing_port_and_explicit_override() {
        let normal = Config::try_parse_from([WATCHER_NAME]).unwrap();
        assert_eq!(normal.server_port(), 5600);
        let testing = Config::try_parse_from([WATCHER_NAME, "--testing"]).unwrap();
        assert_eq!(testing.server_port(), 5666);
        let explicit =
            Config::try_parse_from([WATCHER_NAME, "--testing", "--aw-port", "6000"]).unwrap();
        assert_eq!(explicit.server_port(), 6000);
    }

    #[test]
    fn reject_unsafe_device_ids() {
        for id in ["", "../phone", "phone name", "手机"] {
            assert!(parse_device_id(id).is_err());
        }
        assert_eq!(parse_device_id("pixel-9_pro").unwrap(), "pixel-9_pro");
    }

    #[test]
    fn reject_empty_or_invalid_secrets_and_limits() {
        let mut config = Config::try_parse_from([WATCHER_NAME]).unwrap();
        for token in ["", " ", "secret\nvalue", "非ASCII"] {
            config.token = Some(token.into());
            assert!(config.validate().is_err());
        }
        config.token = Some("valid-token".into());
        config.max_body_bytes = 0;
        assert!(config.validate().is_err());
    }
}
