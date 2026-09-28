#!/usr/bin/env bash
# 自动修复代码风格问题（幂等，可反复运行）。
# 用法：./scripts/format.sh
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

step() { printf '\n── %s\n' "$1"; }

step "1/4 格式化：cargo fmt --all"
cargo fmt --all || {
    printf '\n[失败] cargo fmt 执行失败\n' >&2
    printf '[修复] 若提示 rustfmt/工具链未安装，先运行 ./scripts/setup.sh；若是语法错误，按上面报错修好后再运行本脚本\n' >&2
    exit 1
}

step "2/4 自动修复可自动修的问题：cargo clippy --fix"
# --allow-dirty --allow-staged：工作区通常处于未提交状态，允许在未提交内容上修复
cargo clippy --fix --all-targets --allow-dirty --allow-staged || {
    printf '\n[失败] cargo clippy --fix 执行失败（通常是编译错误）\n[修复] 按上面的报错修好编译错误后重新运行本脚本\n' >&2
    exit 1
}

step "3/4 再格式化一次（clippy 的修复可能改动排版）"
cargo fmt --all

step "4/4 严格校验剩余问题"
if ./scripts/check.sh --quick; then
    printf '\n[完成] 格式与 lint 已全部通过。\n'
else
    printf '\n[未通过] 仍有需要手动处理的问题（见上）。修好后重新运行本脚本。\n' >&2
    exit 1
fi
