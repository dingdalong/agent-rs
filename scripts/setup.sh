#!/usr/bin/env bash
# 首次准备：安装钉死的工具链与组件、启用 git hooks。可重复执行。
# 用法：./scripts/setup.sh
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

step() { printf '\n── %s\n' "$1"; }

if ! command -v rustup >/dev/null 2>&1; then
    printf '[需要手动处理] 没找到 rustup。\n' >&2
    printf '请先安装 Rust 工具链管理器 rustup，例如：\n' >&2
    printf '  brew install rustup-init && rustup-init\n' >&2
    printf '  或参考 https://rustup.rs\n' >&2
    exit 1
fi

step "1/3 安装工具链（版本来自 rust-toolchain.toml）"
rustup show

step "2/3 补装组件：rustfmt / clippy"
rustup component add rustfmt clippy

step "3/3 启用 git hooks 与脚本权限"
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit scripts/setup.sh scripts/format.sh scripts/check.sh scripts/build.sh

step "完成，当前工具链："
rustc --version
rustfmt --version
cargo clippy --version | head -n 1
printf '\n日常用法：改完代码跑 ./scripts/format.sh；需要产物跑 ./scripts/build.sh\n'
