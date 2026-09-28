#!/usr/bin/env bash
# 代码风格与质量校验（只读：不修改任何文件）。
# 用法：
#   ./scripts/check.sh           全量检查（提交钩子、CI）
#   ./scripts/check.sh --quick   跳过测试（供 format.sh 自证格式与 lint 用）
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

QUICK=0
case "${1:-}" in
    "") ;;
    --quick) QUICK=1 ;;
    *)
        printf '未知参数：%s（可用：--quick）\n' "$1" >&2
        exit 2
        ;;
esac

step() { printf '\n── %s\n' "$1"; }
fail() {
    printf '\n[失败] %s\n' "$1" >&2
    printf '[修复] %s\n' "$2" >&2
    exit 1
}

# ── 守卫 1/3：版本同步 ───────────────────────────────────────────────────────
step "守卫 1/3：Cargo.toml 的 rust-version 与 rust-toolchain.toml 的 channel 同步"

rust_version=$(sed -n 's/^rust-version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
channel=$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml | head -n 1)
major_minor() { printf '%s' "$1" | cut -d. -f1,2; }

if [ -z "$rust_version" ] || [ -z "$channel" ]; then
    fail "没有读到版本号（Cargo.toml rust-version='${rust_version}'，rust-toolchain.toml channel='${channel}'）" \
         "确认两个文件都写了对应字段"
fi
if [ "$(major_minor "$rust_version")" != "$(major_minor "$channel")" ]; then
    fail "版本不同步：Cargo.toml rust-version = ${rust_version}，rust-toolchain.toml channel = ${channel}" \
         "把两者改成同一个 major.minor（升级 Rust 时两个一起改）"
fi
printf 'rust-version = %s，channel = %s（一致）\n' "$rust_version" "$channel"

# ── 守卫 2/3：风格值声明与 rustfmt 生效值一致 ────────────────────────────────
step "守卫 2/3：.editorconfig 声明值与 rustfmt 实际生效值一致"

if ! rustfmt --help 2>/dev/null | grep -q -- '--print-config'; then
    fail "当前 rustfmt 不支持 --print-config，无法自动比对风格值" \
         "在 rustfmt.toml 显式写出 tab_spaces / hard_tabs / max_width，并删掉本守卫"
fi

effective=$(rustfmt --print-config current src/main.rs)
effective_value() {
    printf '%s\n' "$effective" | sed -n "s/^$1[[:space:]]*=[[:space:]]*//p" | head -n 1 | tr -d '"'
}
editorconfig_value() {
    awk -v key="$1" '
        # 本文件自身若被写成 CRLF，剥掉回车后按声明解析，行尾问题交由守卫 3 报出
        { sub(/\r$/, "") }
        /^\[/ { in_root = ($0 == "[*]") }
        in_root && $0 ~ "^" key "[[:space:]]*=" {
            sub(/^[^=]*=[[:space:]]*/, "")
            print
            exit
        }
    ' .editorconfig
}

ec_indent_style=$(editorconfig_value indent_style)
ec_indent_size=$(editorconfig_value indent_size)
ec_end_of_line=$(editorconfig_value end_of_line)
ec_max_line_length=$(editorconfig_value max_line_length)
ef_hard_tabs=$(effective_value hard_tabs)
ef_tab_spaces=$(effective_value tab_spaces)
ef_newline_style=$(effective_value newline_style)
ef_max_width=$(effective_value max_width)

if [ -z "$ef_hard_tabs" ] || [ -z "$ef_tab_spaces" ] || [ -z "$ef_newline_style" ] || [ -z "$ef_max_width" ]; then
    fail "没能从 rustfmt --print-config 读到完整的生效值（可能是该版本输出格式不同）" \
         "先手动跑一次 rustfmt --print-config current src/main.rs 看输出，再调整本守卫，或改为在 rustfmt.toml 显式声明这几项"
fi

problems=""
mismatch() {
    problems="${problems}  - ${1}：.editorconfig 声明「${2}」，rustfmt 生效「${3}」"$'\n'
}

{ [ "$ec_indent_style" = "space" ] && [ "$ef_hard_tabs" = "false" ]; } ||
    mismatch "缩进方式（indent_style ↔ hard_tabs）" "$ec_indent_style" "$ef_hard_tabs"
[ "$ef_tab_spaces" = "$ec_indent_size" ] ||
    mismatch "缩进宽度（indent_size ↔ tab_spaces）" "$ec_indent_size" "$ef_tab_spaces"
[ "$ef_max_width" = "$ec_max_line_length" ] ||
    mismatch "最大行宽（max_line_length ↔ max_width）" "$ec_max_line_length" "$ef_max_width"
case "$ec_end_of_line" in
    lf) expected_newline_style="Unix" ;;
    crlf) expected_newline_style="Windows" ;;
    cr) expected_newline_style="Mac" ;;
    *) expected_newline_style="未知" ;;
esac
[ "$ef_newline_style" = "$expected_newline_style" ] ||
    mismatch "行尾（end_of_line ↔ newline_style）" "$ec_end_of_line" "$ef_newline_style"

if [ -n "$problems" ]; then
    printf '%s' "$problems" >&2
    fail "上面这些风格值在 .editorconfig 与 rustfmt 生效值之间不一致" \
         "优先改 .editorconfig 的声明；rustfmt 不读 .editorconfig，若它无法跟随就在 rustfmt.toml 显式写上对应项"
fi
printf '缩进 %s/*%s、行尾 %s、最大行宽 %s（一致）\n' "$ec_indent_style" "$ec_indent_size" "$ec_end_of_line" "$ec_max_line_length"

# ── 守卫 3/3：行尾与文件末尾换行 ─────────────────────────────────────────────
step "守卫 3/3：文件行尾为 LF，且非空文本文件末尾恰好一个换行"

crlf_files=$(git ls-files --eol | grep -E 'i/(crlf|mixed)|w/(crlf|mixed)' || true)
if [ -n "$crlf_files" ]; then
    printf '%s\n' "$crlf_files" >&2
    fail "以上文件使用了 CRLF 或混合行尾（.editorconfig 声明 end_of_line = lf）" \
         ".rs 运行 ./scripts/format.sh 自动改成 LF；其他文件用编辑器另存（已声明 lf）"
fi

eof_problems=""
while IFS= read -r -d '' record; do
    meta=${record%%$'\t'*}
    path=${record#*$'\t'}
    case "${meta%%[[:space:]]*}" in
        i/-text) continue ;; # 二进制文件不套用文本规则
        i/) continue ;;      # 子模块（gitlink）没有行尾信息
    esac
    [ -n "$path" ] || continue
    [ -s "./$path" ] || continue # 空文件豁免

    last_byte=$(tail -c 1 "./$path" | od -An -tu1 | tr -d ' \n')
    if [ "$last_byte" != "10" ]; then
        eof_problems="${eof_problems}  - ${path}：末尾缺少换行"$'\n'
        continue
    fi
    if [ "$(wc -c < "./$path" | tr -d ' ')" -ge 2 ]; then
        second_last=$(tail -c 2 "./$path" | head -c 1 | od -An -tu1 | tr -d ' \n')
        if [ "$second_last" = "10" ]; then
            eof_problems="${eof_problems}  - ${path}：末尾有多个换行"$'\n'
        fi
    fi
done < <(git ls-files --eol -z)

if [ -n "$eof_problems" ]; then
    printf '%s' "$eof_problems" >&2
    fail "以上文件的末尾换行不符合规则（除空文件外，末尾有且仅有一个换行）" \
         ".rs 运行 ./scripts/format.sh 自动修复；其他文件用编辑器另存"
fi

# ── 格式、lint 与测试 ────────────────────────────────────────────────────────
step "格式检查：cargo fmt --all --check"
cargo fmt --all --check ||
    fail "存在未格式化的代码（上面是格式化差异）" "运行 ./scripts/format.sh 自动修复；若提示 rustfmt 未安装，先运行 ./scripts/setup.sh"

step "lint：cargo clippy --all-targets --locked -- -D warnings"
cargo clippy --all-targets --locked -- -D warnings ||
    fail "clippy 报警（已按 -D warnings 视为错误）" "先运行 ./scripts/format.sh 自动修复，剩余项按上面提示手动改"

if [ "$QUICK" -eq 1 ]; then
    printf '\n[通过] 快速校验通过（已跳过测试）。\n'
    exit 0
fi

step "测试：cargo test --locked"
cargo test --locked ||
    fail "测试未通过" "先修测试；若提示 Cargo.lock 与 Cargo.toml 不同步，运行 cargo update 后一并提交 Cargo.lock"

printf '\n[通过] 校验全部通过（守卫 + 格式 + lint + 测试）。\n'
