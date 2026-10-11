# 安装与部署

## 从源码构建和安装

需要 Rust stable（edition 2024）与 Cargo；首次获取 Git 依赖需要网络。

```bash
cargo build --locked --release
cargo install --locked --path .
aw-watcher-healthconnect --help
```

无需 root，程序安装到 Cargo 的 bin 目录，请将该目录加入
PATH。只使用构建产物时运行 `./target/release/aw-watcher-healthconnect`。

## 部署配置

默认监听本机 `127.0.0.1:5700` 并连接 AW `127.0.0.1:5600`。手机局域网访问需
`--host 0.0.0.0`，并建议设置 `AW_HEALTHCONNECT_TOKEN`；手机请求头填写对应
`X-API-Key`。

`--testing` 使用 AW 测试端口 `5666`，不会启动 AW；需要另行运行 `aw-server --testing`。

不同手机使用不同的 `--device-id` 和监听端口，避免不同来源的健康记录混入同一个桶。

在环境文件中设置 `AW_HEALTHCONNECT_TOKEN`，限制文件权限为仅用户可读；不要把真实
token 提交到仓库。示例：

```text
AW_HEALTHCONNECT_TOKEN=replace-with-a-long-random-secret
```

## 安全与检查

- 不要直接暴露公网；跨不可信网络使用 TLS 反向代理
- `AW_API_KEY` 是 AW 服务端凭证，与 Webhook token 不同
- `GET /health` 验证 AW 可达性，只有实际同步成功才验证写入权限
- Watcher 不持久化离线队列；AW 写入失败返回 `503` 供应用重试
- 自定义健康桶可在 AW Raw Data 中查看，不自动生成健康图表

配置、请求格式和已知限制见 `README.md`。本地开发执行 `just check`；无需运行 AW 的集成测试会自动启动模拟 REST 服务。
