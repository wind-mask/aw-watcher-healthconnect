# 快速开始（Rust）

## 1. 构建

```bash
cargo build --locked --release
```

## 2. 启动

先启动 ActivityWatch，再启动本项目。局域网接收需显式开放监听地址：

```bash
export AW_HEALTHCONNECT_TOKEN='replace-with-a-long-random-secret'
./target/release/aw-watcher-healthconnect --host 0.0.0.0 --device-id pixel
```

默认 AW 地址为 `127.0.0.1:5600`。开发测试时改用 `--testing` 并启动 `aw-server --testing`。

## 3. Android 应用设置

HC Webhook → Webhooks：

- URL：`http://电脑局域网IP:5700/webhook`
- Method：`POST`
- Delivery format：`JSON`
- Custom header：`X-API-Key`，填入同一个 token

选择需要的健康类型并授权，再启动实际同步。应用的 Test 按钮只校验格式，不保存模拟数据。

## 4. 验证

```bash
curl http://127.0.0.1:5700/health
just test-webhook
```

成功导入的桶形如 `aw-watcher-healthconnect-activity_pixel`，可在 AW 的 Raw
Data/桶浏览器查看。自定义健康桶不会自动显示为默认 Activity 统计。

遇到 `503` 时检查 AW 服务端连接，手机会按上游机制重试。更多配置与安全注意事项见
`README.md`；全部测试与检查运行 `just check`。
