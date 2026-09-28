# AGENTS.md

为Agent开发做指导。

- 完成代码、文档修改后执行脚本`./scripts/format.sh`格式化（自动修复，可重复运行）；交付前再执行一次。
- 被 pre-commit 钩子拦下时，也先跑`./scripts/format.sh`，其余失败项按报错修好再提交。
日常 debug 迭代用 cargo run，脚本`./scripts/build.sh`用于发布构建。

## 用户编码习惯与要求（强制加载并遵守）

参见：`用户编码习惯与要求.md`
