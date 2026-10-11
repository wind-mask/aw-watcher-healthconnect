# aw-watcher-healthconnect

使用 Rust 编写的 ActivityWatch Watcher：接收
[health-connect-webhook](https://github.com/mcnaveen/health-connect-webhook/) 的
JSON 请求，并通过官方 Rust 客户端写入 ActivityWatch。

## 实现与支持范围

- 使用 `aw-server-rust` 工作区的 `aw-client-rust` 和 `aw-models`（固定 Git 修订与 `Cargo.lock`）。
- Axum/Tokio 接收 `POST /webhook`；仅支持上游 **JSON** 模式，不支持 Protobuf/gRPC
- 支持当前 `SyncManager.buildJsonPayload` 输出的 33 种数据类型，包括文档尚未列出的 `body_water_mass` 和 `bmi`
- 按健康类别自动建桶、批量插入历史事件，不使用 heartbeat 合并独立测量
- 写入前检查服务端现有事件，支持重复请求、重启及部分写入后的重试去重
- 整批校验、请求体/记录数限制、写入超时和可选 `X-API-Key` 认证
- 上游 `test: true` 模拟请求只校验，不保存，避免污染真实健康记录

## 安装与启动

需要 Rust stable（支持 edition 2024）及运行中的 ActivityWatch。依赖有 Git 来源，首次构建需要网络。

```bash
cargo build --locked --release
# 安装到 Cargo 的 bin 目录，无需 root
cargo install --locked --path .

# 默认仅监听本机 127.0.0.1:5700，连接 AW 127.0.0.1:5600
aw-watcher-healthconnect

# 连接 AW 测试端口 5666，需另行启动 aw-server --testing
aw-watcher-healthconnect --testing
```

项目任务使用 `just` + Nushell：`just run`、`just build`、`just test`、`just check`。

## 手机配置

为手机开启局域网接收入口，建议配置随机长 token。以下为 bash 示例；不要把真实 token 提交到仓库：

```bash
export AW_HEALTHCONNECT_TOKEN='replace-with-a-long-random-secret'
aw-watcher-healthconnect --host 0.0.0.0 --device-id pixel
```

HC Webhook 应用设置：

1. URL：`http://电脑局域网IP:5700/webhook`
2. 方法：`POST`；Delivery format：**JSON**
3. Custom headers：`X-API-Key`，值与 `AW_HEALTHCONNECT_TOKEN` 相同
4. 选择需要的数据类型，授予 Health Connect 读取权限，配置同步间隔/计划
5. 点击 Test 校验接收格式；随后实际同步才会写入事件

多台手机应分别运行 Watcher，使用不同的 `--device-id`、端口和
token。设备标识必须为 1–64 个 ASCII 字母、数字、连字符或下划线。

## 配置

| CLI 参数 | 环境变量 | 默认值 |
| --- | --- | --- |
| `--host` | `AW_HEALTHCONNECT_HOST` | `127.0.0.1` |
| `--port` | `AW_HEALTHCONNECT_PORT` | `5700` |
| `--aw-host` | `AW_HOST` | `127.0.0.1` |
| `--aw-port` | `AW_PORT` | `5600`；`--testing` 时为 `5666` |
| `--aw-protocol` | `AW_PROTOCOL` | `http`（可选 `https`） |
| `--device-id` | `AW_HEALTHCONNECT_DEVICE_ID` | `android` |
| `--token` | `AW_HEALTHCONNECT_TOKEN` | 无，校验手机的 `X-API-Key` |
| `--aw-api-key` | `AW_API_KEY` | 无，用于 AW 服务端认证 |
| `--request-timeout` | `AW_HEALTHCONNECT_REQUEST_TIMEOUT` | `30` 秒，含上传及等待并发写入 |
| `--max-body-bytes` | `AW_HEALTHCONNECT_MAX_BODY_BYTES` | `16777216`（16 MiB） |
| `--verbose` | `RUST_LOG` 可配置日志过滤器 | 关闭详细日志 |

`--token` 与 `--aw-api-key` 是两种独立凭证。`--testing`
不代表丢弃健康数据，而是连接测试 AW
实例。未显式设置端口时的生产/测试默认值如上。

## HTTP 接口

- `GET /`：名称、版本与支持的数据类型（存活检查）
- `GET /health`：ActivityWatch 可达性检查，可达为 `200`，不可达为 `503`；不验证写入权限
- `POST /webhook`：接收 JSON；成功为 `200`，返回 `received`、`written`、`duplicates` 和各桶写入数量
- 未认证：`401`；不符合 JSON/记录格式：`400` / `415` / `422`；请求体超限：`413`
- AW 失败/超时：`503` + `Retry-After: 5`，让应用按其机制重试；不会将未确认写入当作成功

AW 可以晚于 Watcher
启动。没有本地离线队列，失败批次依赖手机重试。只有整批成功（或模拟测试通过）才返回
2xx。跨桶不是事务，失败前已写入的记录会在重试时去重。

单个请求的响应期限包含上传/解析及等待写入。最多同时接收 4 个批次，超额返回
`503`；慢上传返回 `408`。响应超时不会取消已启动的 AW
写入，后续批次继续等待写锁，减少迟到提交导致的重复风险。普通网络断连或客户端内部超时仍存在结果不确定性，AW
API 不提供跨进程的幂等键/事务，本项目不声称严格 exactly-once。

## 数据模型

桶
ID：`aw-watcher-healthconnect-<category>_<device-id>`，桶类型：`healthconnect.<category>`；`hostname`
为来源设备标识。

| 类别 | 类型 |
| --- | --- |
| activity | steps、distance、active_calories、total_calories |
| vitals | heart_rate、heart_rate_variability、oxygen_saturation、respiratory_rate、body_temperature、skin_temperature、resting_heart_rate |
| body | weight、height、body_fat、lean_body_mass、bone_mass、body_water_mass、bmi、basal_metabolic_rate、vo2_max |
| health | blood_pressure、blood_glucose、hydration |
| sleep | sleep |
| exercise | exercise |
| nutrition | nutrition |
| women | menstruation_flow、menstruation_period、intermenstrual_bleeding、ovulation_test、cervical_mucus、sexual_activity、basal_body_temperature |

事件 `data` 完整保留原记录，并增加 `data_type`，因此运动记录的 `type` 仍是原始运动类型。时间规则：

- 区间记录：`timestamp = start_time`，`duration = end_time - start_time`
- 睡眠：`timestamp = session_end_time - duration_seconds`，保留嵌套的 `stages`，不额外生成重复的阶段事件
- 测量与聚合样本：使用 `time`，时长为 0；上游未提供聚合窗口宽度，不能臆造持续时间
- 不使用信封发送时间代替记录时间，不把同步时间/应用版本加入健康事件，避免每次重试都产生不同内容

去重按完整记录内容进行，不自动覆盖发生修改的旧健康记录：内容变化会追加一个版本。上游 JSON 当前未可靠发送记录身份/修改时间，不能安全地实现通用 upsert 或删除同步。

单批最多 25,000 条源记录（不含派生 BMI），含 BMI 的总硬上限为 50,000
条。未知的非空数据类型会拒绝批次，避免静默丢失；空数组可接受。去重查询最多容纳
100,000
条已有事件，超过时拒绝写入并要求缩小手机同步范围，不基于截断结果进行去重。

自定义健康桶可在 ActivityWatch 的 Raw Data/桶浏览器中查看，但不会自动成为默认
Activity 页面中的活动统计；专用健康图表需要另行配置或开发。

## 测试与开发

```bash
cargo test --locked --all-targets
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
# 或 just check
```

HTTP 集成测试使用真实 `aw-client-rust` 访问本地模拟 AW REST，不需要正在运行的 AW，也不会写入真实健康数据。覆盖重复导入、重启、并发、部分提交、超时恢复、认证、多设备隔离及批量限制。

`examples/webhook.json` 是合成样例，含 `test: true`：

```bash
just test-webhook
just health
```

将 `test` 改成 `false` 才会保存，验证实际写入时请连接 AW 测试实例。

## 安全与限制

健康数据敏感，默认监听回环地址；局域网开放入口应配置
token，不要直接暴露公网。HTTP 不加密，需要跨不可信网络时使用可信 TLS
反向代理。正常日志不打印健康记录或凭证；开启依赖的 trace 日志时请自行评估风险。

没有启动系统服务、修改防火墙或自动安装到系统目录。现有目录未初始化 Git；已提供
`.pre-commit-config.yaml` 和 `.cz.toml`，初始化仓库后可运行
`just install-hooks`。

MIT，见 `LICENSE`。本项目仅按上游协议互操作，不包含上游 Android 应用代码。
