use std::collections::BTreeMap;

use aw_models::Event;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
use thiserror::Error;

/// 上游先限制源记录数，再生成 BMI；为派生记录留出空间，并另有请求字节数限制。
pub const MAX_SOURCE_RECORDS: usize = 25_000;
pub const MAX_RECORDS: usize = 50_000;

#[derive(Deserialize)]
pub struct Payload {
    pub timestamp: DateTime<Utc>,
    pub app_version: String,
    #[serde(default)]
    pub test: bool,
    #[serde(flatten)]
    pub records: BTreeMap<String, Value>,
}

pub struct ConvertedPayload {
    pub events: BTreeMap<&'static str, Vec<Event>>,
    pub record_count: usize,
}

#[derive(Debug, Error)]
#[error("{path}: {message}")]
pub struct ValidationError {
    pub path: String,
    pub message: &'static str,
}

fn invalid(path: impl Into<String>, message: &'static str) -> ValidationError {
    ValidationError {
        path: path.into(),
        message,
    }
}

#[derive(Clone, Copy)]
enum Shape {
    Instant(&'static [&'static str]),
    Interval(&'static [&'static str]),
    Series {
        sample: &'static str,
        average: &'static str,
        minimum: &'static str,
        maximum: &'static str,
    },
    Sleep,
}

pub struct DataType {
    pub name: &'static str,
    pub category: &'static str,
    shape: Shape,
}

const fn instant(
    name: &'static str,
    category: &'static str,
    fields: &'static [&'static str],
) -> DataType {
    DataType {
        name,
        category,
        shape: Shape::Instant(fields),
    }
}

const fn interval(
    name: &'static str,
    category: &'static str,
    fields: &'static [&'static str],
) -> DataType {
    DataType {
        name,
        category,
        shape: Shape::Interval(fields),
    }
}

const fn series(name: &'static str, sample: &'static str) -> DataType {
    DataType {
        name,
        category: "vitals",
        shape: Shape::Series {
            sample,
            average: "avg",
            minimum: "min",
            maximum: "max",
        },
    }
}

/// 以 SyncManager.buildJsonPayload 为准，包含文档尚未列出的 body_water_mass 和 bmi。
pub const DATA_TYPES: &[DataType] = &[
    interval("steps", "activity", &["count"]),
    interval("distance", "activity", &["meters"]),
    interval("active_calories", "activity", &["calories"]),
    interval("total_calories", "activity", &["calories"]),
    DataType {
        name: "sleep",
        category: "sleep",
        shape: Shape::Sleep,
    },
    series("heart_rate", "bpm"),
    series("heart_rate_variability", "rmssd_millis"),
    series("oxygen_saturation", "percentage"),
    series("respiratory_rate", "rate"),
    DataType {
        name: "skin_temperature",
        category: "vitals",
        shape: Shape::Series {
            sample: "delta_celsius",
            average: "avg_delta_celsius",
            minimum: "min_delta_celsius",
            maximum: "max_delta_celsius",
        },
    },
    instant("body_temperature", "vitals", &["celsius"]),
    instant("resting_heart_rate", "vitals", &["bpm"]),
    instant("weight", "body", &["kilograms"]),
    instant("height", "body", &["meters"]),
    instant("body_fat", "body", &["percentage"]),
    instant("lean_body_mass", "body", &["kilograms"]),
    instant("bone_mass", "body", &["kilograms"]),
    instant("body_water_mass", "body", &["kilograms"]),
    instant("bmi", "body", &["value", "weight_kg", "height_meters"]),
    instant("basal_metabolic_rate", "body", &["watts"]),
    instant("vo2_max", "body", &["ml_per_kg_per_min"]),
    instant("blood_pressure", "health", &["systolic", "diastolic"]),
    instant("blood_glucose", "health", &["mmol_per_liter"]),
    interval("hydration", "health", &["liters"]),
    interval("exercise", "exercise", &[]),
    interval("nutrition", "nutrition", &[]),
    instant("menstruation_flow", "women", &["flow"]),
    interval("menstruation_period", "women", &[]),
    instant("intermenstrual_bleeding", "women", &[]),
    instant("ovulation_test", "women", &["result"]),
    instant("cervical_mucus", "women", &["appearance"]),
    instant("sexual_activity", "women", &["protection_used"]),
    instant(
        "basal_body_temperature",
        "women",
        &["celsius", "measurement_location"],
    ),
];

pub fn convert(payload: &Payload) -> Result<ConvertedPayload, ValidationError> {
    if payload.timestamp.timestamp_nanos_opt().is_none() {
        return Err(invalid("timestamp", "超出 ActivityWatch 纳秒时间戳范围"));
    }
    if payload.app_version.trim().is_empty() || payload.app_version.len() > 128 {
        return Err(invalid("app_version", "需为 1–128 字节的非空字符串"));
    }

    let mut result = ConvertedPayload {
        events: BTreeMap::new(),
        record_count: 0,
    };
    let mut source_count = 0;
    // 先验证整个批次，任何一条错误都不会触发部分写入。
    for (name, value) in &payload.records {
        let records = value
            .as_array()
            .ok_or_else(|| invalid(name, "健康数据必须是数组"))?;
        result.record_count += records.len();
        if name != "bmi" {
            source_count += records.len();
        }
        if source_count > MAX_SOURCE_RECORDS {
            return Err(invalid(
                name,
                "单次请求最多支持 25000 条源健康记录（不含派生 BMI）",
            ));
        }
        if result.record_count > MAX_RECORDS {
            return Err(invalid(
                name,
                "单次请求最多支持 50000 条健康记录（含派生 BMI）",
            ));
        }
        let spec = DATA_TYPES.iter().find(|spec| spec.name == name);
        let Some(spec) = spec else {
            if records.is_empty() {
                continue;
            }
            return Err(invalid(
                name,
                "不支持的健康数据类型；拒绝批次以避免静默丢失数据",
            ));
        };
        for (index, record) in records.iter().enumerate() {
            let path = format!("{name}[{index}]");
            let event = convert_record(spec, record, &path)?;
            result.events.entry(spec.category).or_default().push(event);
        }
    }
    Ok(result)
}

fn convert_record(spec: &DataType, value: &Value, path: &str) -> Result<Event, ValidationError> {
    let record = value
        .as_object()
        .ok_or_else(|| invalid(path, "记录必须是 JSON 对象"))?;
    if record.contains_key("data_type") {
        return Err(invalid(
            format!("{path}.data_type"),
            "该字段保留给 Watcher 的数据类型标识",
        ));
    }
    let (timestamp, duration) = match spec.shape {
        Shape::Instant(fields) => {
            require_numbers(record, fields, path)?;
            (time(record, "time", path)?, Duration::zero())
        }
        Shape::Interval(fields) => {
            require_numbers(record, fields, path)?;
            interval_time(record, path)?
        }
        Shape::Series {
            sample,
            average,
            minimum,
            maximum,
        } => {
            let aggregate = [average, minimum, maximum]
                .iter()
                .any(|key| record.contains_key(*key));
            if aggregate {
                require_numbers(record, &[average, minimum, maximum], path)?;
                if record.contains_key(sample) {
                    require_numbers(record, &[sample], path)?;
                }
            } else {
                require_numbers(record, &[sample], path)?;
            }
            // 上游不发送聚合窗口宽度，不能把平均值错误地当成一分钟持续事件。
            (time(record, "time", path)?, Duration::zero())
        }
        Shape::Sleep => {
            let end = time(record, "session_end_time", path)?;
            let seconds = record
                .get("duration_seconds")
                .and_then(Value::as_i64)
                .filter(|seconds| *seconds >= 0)
                .ok_or_else(|| invalid(format!("{path}.duration_seconds"), "需为非负整数秒"))?;
            let nanos = seconds.checked_mul(1_000_000_000).ok_or_else(|| {
                invalid(format!("{path}.duration_seconds"), "时长超出纳秒表示范围")
            })?;
            let duration = Duration::nanoseconds(nanos);
            let start = end
                .checked_sub_signed(duration)
                .filter(|start| start.timestamp_nanos_opt().is_some())
                .ok_or_else(|| invalid(path, "反推的睡眠开始时间超出支持范围"))?;
            if let Some(stages) = record.get("stages") {
                let stages = stages
                    .as_array()
                    .ok_or_else(|| invalid(format!("{path}.stages"), "需为数组"))?;
                for (index, stage) in stages.iter().enumerate() {
                    let stage_path = format!("{path}.stages[{index}]");
                    let stage = stage
                        .as_object()
                        .ok_or_else(|| invalid(&stage_path, "需为对象"))?;
                    require_string(stage, "stage", &stage_path)?;
                    interval_time(stage, &stage_path)?;
                }
            }
            (start, duration)
        }
    };

    if spec.name == "steps" && record.get("count").and_then(Value::as_u64).is_none() {
        return Err(invalid(format!("{path}.count"), "步数需为非负整数"));
    }
    if spec.name == "exercise" {
        require_string(record, "type", path)?;
    }
    if let Some(seconds) = record.get("duration_seconds")
        && !seconds.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0)
    {
        return Err(invalid(
            format!("{path}.duration_seconds"),
            "时长需为非负数",
        ));
    }

    // 保留原始字段、单位、来源元数据和睡眠阶段；避免覆盖 exercise 的原始 type。
    let mut data = record.clone();
    data.insert("data_type".into(), spec.name.into());
    Ok(Event::new(timestamp, duration, data))
}

fn require_numbers(
    record: &Map<String, Value>,
    fields: &[&str],
    path: &str,
) -> Result<(), ValidationError> {
    for field in fields {
        if !record
            .get(*field)
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
        {
            return Err(invalid(format!("{path}.{field}"), "需为有限数值"));
        }
    }
    Ok(())
}

fn require_string(
    record: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), ValidationError> {
    if !record
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|v| !v.is_empty())
    {
        return Err(invalid(format!("{path}.{field}"), "需为非空字符串"));
    }
    Ok(())
}

fn time(
    record: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<DateTime<Utc>, ValidationError> {
    let value = record
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{path}.{field}"), "需为带时区的 RFC3339 字符串"))?;
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .ok()
        .filter(|time| time.timestamp_nanos_opt().is_some())
        .ok_or_else(|| {
            invalid(
                format!("{path}.{field}"),
                "RFC3339 时间无效或超出 ActivityWatch 支持范围",
            )
        })
}

fn interval_time(
    record: &Map<String, Value>,
    path: &str,
) -> Result<(DateTime<Utc>, Duration), ValidationError> {
    let start = time(record, "start_time", path)?;
    let end = time(record, "end_time", path)?;
    let duration = end.signed_duration_since(start);
    if duration < Duration::zero() {
        return Err(invalid(
            format!("{path}.end_time"),
            "结束时间不能早于开始时间",
        ));
    }
    if duration.num_nanoseconds().is_none() {
        return Err(invalid(path, "时长超出 ActivityWatch 纳秒表示范围"));
    }
    Ok((start, duration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(records: Value) -> Payload {
        let mut value = records.as_object().unwrap().clone();
        value.insert("timestamp".into(), json!("2026-05-09T14:00:00.123Z"));
        value.insert("app_version".into(), json!("1.2.3"));
        serde_json::from_value(Value::Object(value)).unwrap()
    }

    #[test]
    fn all_upstream_types_have_correct_bucket_and_time_shape() {
        assert_eq!(DATA_TYPES.len(), 33);
        for spec in DATA_TYPES {
            let mut record = Map::new();
            match spec.shape {
                Shape::Instant(fields) | Shape::Interval(fields) => {
                    for field in fields {
                        record.insert((*field).into(), json!(1));
                    }
                }
                Shape::Series { sample, .. } => {
                    record.insert(sample.into(), json!(72));
                }
                Shape::Sleep => {
                    record.insert("session_end_time".into(), json!("2026-05-09T08:00:00Z"));
                    record.insert("duration_seconds".into(), json!(3600));
                    record.insert("stages".into(), json!([]));
                }
            }
            if matches!(spec.shape, Shape::Interval(_)) {
                record.insert("start_time".into(), json!("2026-05-09T07:00:00Z"));
                record.insert("end_time".into(), json!("2026-05-09T08:00:00Z"));
            } else if !matches!(spec.shape, Shape::Sleep) {
                record.insert("time".into(), json!("2026-05-09T07:00:00Z"));
            }
            if spec.name == "exercise" {
                record.insert("type".into(), json!("running"));
            }
            let input = payload(json!({spec.name: [record]}));
            let output = convert(&input).unwrap();
            assert_eq!(output.record_count, 1, "{}", spec.name);
            let event = &output.events[spec.category][0];
            assert_eq!(event.data["data_type"], spec.name);
            assert_eq!(event.timestamp.to_rfc3339(), "2026-05-09T07:00:00+00:00");
            let expected = if matches!(spec.shape, Shape::Interval(_) | Shape::Sleep) {
                3600
            } else {
                0
            };
            assert_eq!(event.duration.num_seconds(), expected, "{}", spec.name);
            assert!(event.id.is_none());
        }
    }

    #[test]
    fn preserve_exercise_type_and_metadata() {
        let output = convert(&payload(json!({
            "exercise": [{
                "type": "running", "title": "晨跑",
                "start_time": "2026-05-09T07:00:00Z", "end_time": "2026-05-09T07:30:00Z",
                "distance_meters": 5000,
                "metadata": {"id": "record-1", "data_origin": "com.samsung.android.app.health"}
            }]
        })))
        .unwrap();
        let event = &output.events["exercise"][0];
        assert_eq!(event.data["type"], "running");
        assert_eq!(event.data["title"], "晨跑");
        assert_eq!(event.data["data_type"], "exercise");
        assert_eq!(event.data["metadata"]["id"], "record-1");
        assert_eq!(event.duration.num_seconds(), 1800);
    }

    #[test]
    fn sleep_starts_before_session_end_and_keeps_stages() {
        let output = convert(&payload(json!({
            "sleep": [{
                "session_end_time": "2026-05-09T07:00:00Z", "duration_seconds": 28800,
                "stages": [{"stage":"deep", "start_time":"2026-05-09T00:00:00Z",
                    "end_time":"2026-05-09T01:00:00Z", "duration_seconds":3600}]
            }]
        })))
        .unwrap();
        let event = &output.events["sleep"][0];
        assert_eq!(event.timestamp.to_rfc3339(), "2026-05-08T23:00:00+00:00");
        assert_eq!(event.duration.num_seconds(), 28800);
        assert_eq!(event.data["stages"][0]["stage"], "deep");
    }

    #[test]
    fn aggregation_with_or_without_legacy_sample_alias() {
        let output = convert(&payload(json!({
            "heart_rate": [
                {"time":"2026-05-09T08:15:00Z", "avg":72, "min":68, "max":80},
                {"time":"2026-05-09T08:16:00Z", "avg":73, "min":69, "max":81, "bpm":73}
            ],
            "skin_temperature": [{
                "time":"2026-05-09T08:15:00Z",
                "avg_delta_celsius":0.3, "min_delta_celsius":0.1, "max_delta_celsius":0.5,
                "baseline_celsius":33.0, "measurement_location":3
            }]
        })))
        .unwrap();
        assert_eq!(output.record_count, 3);
        assert!(
            output.events["vitals"]
                .iter()
                .all(|event| event.duration.is_zero())
        );
        assert_eq!(output.events["vitals"][0].data["avg"], 72);
    }

    #[test]
    fn intervals_keep_fractional_seconds_and_normalize_timezone() {
        let output = convert(&payload(json!({
            "steps":[{"count":10,"start_time":"2026-05-09T16:00:00.123456789+08:00",
                "end_time":"2026-05-09T16:00:00.623456789+08:00"}]
        })))
        .unwrap();
        let event = &output.events["activity"][0];
        assert_eq!(
            event.timestamp.to_rfc3339(),
            "2026-05-09T08:00:00.123456789+00:00"
        );
        assert_eq!(event.duration.num_nanoseconds(), Some(500_000_000));
        assert_eq!(serde_json::to_value(event).unwrap()["duration"], 0.5);
    }

    #[test]
    fn invalid_records_reject_entire_batch_without_using_envelope_time() {
        let cases = [
            json!({"steps": {"count":1}}),
            json!({"steps": [null]}),
            json!({"steps": [{"count":1,"start_time":"2026-05-09T08:00:00Z"}]}),
            json!({"steps": [{"count":1,"start_time":"2026-05-09T08:00:00Z","end_time":"2026-05-09T07:00:00Z"}]}),
            json!({"heart_rate":[{"bpm":72}]}),
            json!({"heart_rate":[{"bpm":"72","time":"2026-05-09T07:00:00Z"}]}),
            json!({"heart_rate":[{"avg":72,"time":"2026-05-09T07:00:00Z"}]}),
            json!({"weight":[{"kilograms":60,"time":"2026-05-09T07:00:00"}]}),
            json!({"weight":[{"kilograms":60,"time":"9999-01-01T00:00:00Z"}]}),
            json!({"weight":[{"kilograms":60,"time":"2026-05-09T07:00:00Z","data_type":"fake"}]}),
            json!({"sleep":[{"session_end_time":"2026-05-09T07:00:00Z","duration_seconds":-1}]}),
            json!({"sleep":[{"session_end_time":"2026-05-09T07:00:00Z","duration_seconds":i64::MAX}]}),
            json!({"future_metric":[{"time":"2026-05-09T07:00:00Z","value":10}]}),
        ];
        for case in cases {
            assert!(convert(&payload(case.clone())).is_err(), "{case}");
        }
    }

    #[test]
    fn empty_sync_and_record_limit() {
        assert_eq!(convert(&payload(json!({}))).unwrap().record_count, 0);
        assert_eq!(
            convert(&payload(json!({"steps":[], "future_metric":[]})))
                .unwrap()
                .record_count,
            0
        );
        let record = json!({"kilograms":60,"time":"2026-05-09T07:00:00Z"});
        assert!(
            convert(&payload(
                json!({"weight": vec![record; MAX_SOURCE_RECORDS + 1]})
            ))
            .is_err()
        );
    }

    #[test]
    fn upstream_source_limit_allows_additional_derived_bmi() {
        let record = json!({"bpm":72,"time":"2026-05-09T07:00:00Z"});
        let input = payload(json!({
            "heart_rate": vec![record; MAX_SOURCE_RECORDS - 2],
            "weight": [{"kilograms":70,"time":"2026-05-09T07:00:00Z"}],
            "height": [{"meters":1.75,"time":"2026-05-09T07:00:00Z"}],
            "bmi": [{"value":22.86,"weight_kg":70,"height_meters":1.75,"time":"2026-05-09T07:00:00Z"}]
        }));
        assert_eq!(
            convert(&input).unwrap().record_count,
            MAX_SOURCE_RECORDS + 1
        );
    }
}
