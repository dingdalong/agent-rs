#!/usr/bin/env bash
# 产出可运行的 release 产物（人工用产物验证、CI 构建门禁用）。
# 日常 debug 迭代用 cargo run；本脚本只做发布构建。
# 用法：./scripts/build.sh
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

step() { printf '\n── %s\n' "$1"; }

bin_name=$(sed -n 's/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)

step "发布构建：cargo build --release --locked"
cargo build --release --locked || {
    printf '\n[失败] release 构建失败\n' >&2
    printf '[修复] 按上面的编译错误修复；若提示 Cargo.lock 与 Cargo.toml 不同步，运行 cargo update 后一并提交 Cargo.lock\n' >&2
    exit 1
}

printf '\n[完成] 产物：target/release/%s\n' "$bin_name"
