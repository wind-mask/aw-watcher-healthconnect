set shell := ["nu", "-c"]

# 显示所有任务
default:
    @just --list

# 安装 Rust 可执行文件到 Cargo 的 bin 目录
install:
    cargo install --locked --path .

# 运行 Rust Watcher；可追加命令行参数
run *ARGS:
    cargo run --locked -- {{ARGS}}

# 连接 ActivityWatch 测试端口 5666
run-test *ARGS:
    cargo run --locked -- --testing {{ARGS}}

# 构建发布版本
build:
    cargo build --locked --release

# 单元测试和模拟 AW REST 集成测试
test:
    cargo test --locked --all-targets

# 格式化 Rust 代码
format:
    cargo fmt --all

# 格式和严格 lint 检查
lint:
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings

# 完整开发检查
check: lint test

# 发送安全的模拟 Webhook（test: true，不写入 ActivityWatch）
test-webhook URL="http://127.0.0.1:5700/webhook":
    let token = ($env | get --optional AW_HEALTHCONNECT_TOKEN | default ""); let headers = if $token == "" { {} } else { {"X-API-Key": $token} }; http post --content-type application/json --headers $headers "{{URL}}" (open examples/webhook.json)

# 检查 Watcher 和 ActivityWatch 的连接
health URL="http://127.0.0.1:5700/health":
    http get "{{URL}}"

# 清理 Rust 构建产物
clean:
    cargo clean

# 在初始化 Git 仓库后安装提交检查
install-hooks:
    prek install
