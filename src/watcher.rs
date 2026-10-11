use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use anyhow::{Context, bail};
use aw_client_rust::AwClient;
use aw_models::{Bucket, BucketMetadata, Event};
use chrono::Duration;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{WATCHER_NAME, payload::ConvertedPayload};

const INSERT_BATCH_SIZE: usize = 500;
/// 截断的查询不能用于去重；超过安全上限时拒绝写入而非错误地重复导入。
const MAX_EXISTING_EVENTS: u64 = 100_000;

pub struct Watcher {
    client: AwClient,
    device_id: String,
    write_lock: Arc<Mutex<()>>,
}

#[derive(Default, Serialize)]
pub struct WriteResult {
    pub written: usize,
    pub duplicates: usize,
    pub buckets: BTreeMap<String, usize>,
}

impl Watcher {
    pub fn new(client: AwClient, device_id: String) -> Self {
        Self {
            client,
            device_id,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub async fn check_connection(&self) -> anyhow::Result<()> {
        self.client
            .get_info()
            .await
            .context("ActivityWatch 健康检查失败")?;
        Ok(())
    }

    pub fn bucket_id(&self, category: &str) -> String {
        format!("{WATCHER_NAME}-{category}_{}", self.device_id)
    }

    async fn ensure_bucket(&self, id: &str, category: &str) -> anyhow::Result<()> {
        let event_type = format!("healthconnect.{category}");
        match self.client.get_bucket(id).await {
            Ok(bucket) => {
                if bucket._type != event_type
                    || bucket.client != WATCHER_NAME
                    || bucket.hostname != self.device_id
                {
                    bail!("桶 {id} 已存在，但类型、客户端或来源设备与本 Watcher 不匹配");
                }
            }
            Err(error) if error.status().is_some_and(|status| status.as_u16() == 404) => {
                let bucket = Bucket {
                    bid: None,
                    id: id.into(),
                    client: WATCHER_NAME.into(),
                    _type: event_type,
                    hostname: self.device_id.clone(),
                    data: serde_json::Map::new(),
                    metadata: BucketMetadata::default(),
                    events: None,
                    created: None,
                    last_updated: None,
                };
                self.client
                    .create_bucket(&bucket)
                    .await
                    .context("创建健康数据桶失败")?;
            }
            Err(error) => return Err(error).context("读取 ActivityWatch 桶失败"),
        }
        Ok(())
    }

    pub async fn write(self: Arc<Self>, payload: ConvertedPayload) -> anyhow::Result<WriteResult> {
        // 等待锁可安全取消；真正写入启动后即使 HTTP 超时也必须保留锁到最终响应，
        // 避免旧请求还在服务端提交、重试却提前查询并再次插入。
        let guard = self.write_lock.clone().lock_owned().await;
        let task = tokio::spawn(async move {
            let _guard = guard;
            let result = self.write_locked(payload).await;
            if result.is_err() {
                tracing::warn!("ActivityWatch 后台写入失败；下一次同步可重试");
            }
            result
        });
        task.await.context("ActivityWatch 写入任务异常终止")?
    }

    pub async fn wait_for_idle(&self) {
        let _guard = self.write_lock.lock().await;
    }

    async fn write_locked(&self, payload: ConvertedPayload) -> anyhow::Result<WriteResult> {
        let mut result = WriteResult::default();

        for (category, events) in payload.events {
            let id = self.bucket_id(category);
            self.ensure_bucket(&id, category).await?;

            let min_time = events
                .iter()
                .map(|event| event.timestamp)
                .min()
                .context("空事件桶")?;
            let max_time = events
                .iter()
                .map(Event::calculate_endtime)
                .max()
                .context("空事件桶")?;
            // 外扩一秒：兼容 Python AW 的毫秒精度和开区间端点，覆盖零时长测量。
            let start = min_time
                .checked_sub_signed(Duration::seconds(1))
                .filter(|time| time.timestamp_nanos_opt().is_some());
            let end = max_time
                .checked_add_signed(Duration::seconds(1))
                .filter(|time| time.timestamp_nanos_opt().is_some());
            let existing = self
                .client
                .get_events(&id, start, end, Some(MAX_EXISTING_EVENTS + 1))
                .await
                .context("读取已存事件以防止重复导入失败")?;
            if existing.len() as u64 > MAX_EXISTING_EVENTS {
                bail!(
                    "桶 {id} 的查询窗口超过 {MAX_EXISTING_EVENTS} 条事件，请在手机端缩小同步范围"
                );
            }

            let mut seen = existing
                .iter()
                .map(fingerprint)
                .collect::<anyhow::Result<HashSet<_>>>()?;
            let mut pending = Vec::new();
            for event in events {
                if seen.insert(fingerprint(&event)?) {
                    pending.push(event);
                } else {
                    result.duplicates += 1;
                }
            }
            let count = pending.len();
            for batch in pending.chunks(INSERT_BATCH_SIZE) {
                // 历史记录不能使用 heartbeat，否则同值但独立的测量可能被错误地合并。
                self.client
                    .insert_events(&id, batch.to_vec())
                    .await
                    .context("插入健康事件失败")?;
            }
            result.written += count;
            result.buckets.insert(id, count);
        }
        Ok(result)
    }
}

/// 时间字段完整保留在 data 中；只以原始记录内容去重。
/// Python AW 会将事件 timestamp 截断到毫秒，区间查询也可能裁剪事件；
/// 因此不能把服务端规范化后的 timestamp/duration 或事件 ID 当作去重依据。
fn fingerprint(event: &Event) -> anyhow::Result<[u8; 32]> {
    let mut value = serde_json::to_value(&event.data)?;
    value.sort_all_objects();
    Ok(Sha256::digest(serde_json::to_vec(&value)?).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn server_id_time_rounding_and_json_order_do_not_affect_deduplication() {
        let mut a = Event::new(
            Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            Duration::zero(),
            serde_json::from_str(r#"{"b":2,"a":{"d":4,"c":3}}"#).unwrap(),
        );
        let mut b = Event::new(
            a.timestamp,
            a.duration,
            serde_json::from_str(r#"{"a":{"c":3,"d":4},"b":2}"#).unwrap(),
        );
        a.id = Some(1);
        b.id = Some(999);
        b.timestamp += Duration::milliseconds(1);
        b.duration = Duration::milliseconds(1);
        assert_eq!(fingerprint(&a).unwrap(), fingerprint(&b).unwrap());
        b.data.insert("b".into(), 3.into());
        assert_ne!(fingerprint(&a).unwrap(), fingerprint(&b).unwrap());
    }
}
