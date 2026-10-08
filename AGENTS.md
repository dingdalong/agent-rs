# AGENTS.md

为Agent开发做指导。

- 当前项目未正式上线，重构时不考虑兼容性和系统过渡期。
- 用户有 C/C++、Lua 语言基础，熟悉面向对象的开发模式。
- 用户对 Rust 语言不熟悉，在遇到语法报错时，需要解释相关语法。

- 完成代码、文档修改后执行脚本`./scripts/format.sh`格式化（自动修复，可重复运行）；交付前再执行一次。
- 被 pre-commit 钩子拦下时，也先跑`./scripts/format.sh`，其余失败项按报错修好再提交。
日常 debug 迭代用 cargo run，脚本`./scripts/build.sh`用于发布构建。

## 测试用例布局

- 单元测试统一放在仓库根目录的 `unit_tests/` 下，按被测模块路径镜像：`src/llm/deepseek.rs` 的测试放 `unit_tests/llm/deepseek.rs`。
- 源文件末尾统一写 `#[cfg(test)]` + `#[path = "…"]` + `mod tests;`；`#[path]` 相对声明所在文件目录（`src/config.rs` 用 `../unit_tests/config.rs`，`src/llm/mod.rs` 用 `../../unit_tests/llm.rs`）。
- 不要用仓库根 `tests/`（那是 Cargo 集成测试目录，顶层文件会被编成独立 crate、拿不到私有项），也不要新建 `src/<模块>/tests.rs`、不写内联测试块、不在声明处加说明注释。

## 用户编码习惯与要求（强制加载并遵守）

参见：`用户编码习惯与要求.md`
