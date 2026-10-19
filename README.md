# Agent

## 环境配置

- 运行 `./scripts/setup.sh` 安装环境工具。
- 运行 `./scripts/format.sh` 格式化代码。
- 运行 `./scripts/check.sh` 检查格式、运行测试用例。
- 运行 `./scripts/build.sh` 构建发布产物。
- 日常调试编译运行 `cargo run`。

## 运行配置

程序只读取当前工作目录下的 `config.toml` 与 `auth.toml`，请在仓库根目录运行。

- `config.toml`：提交入库。`[llm]` 分节的 `default_model` 指定默认模型规则，规则形如 `provider/model`；`[llm.providers.<名字>]` 分节声明各 provider 的 `url`。
- `auth.toml`：本地凭据，已被 `.gitignore` 忽略。可按 `auth.toml.example` 复制创建，用同样的 `[llm.providers.<名字>]` 分节填 `api_key`。

启动时会检查两个配置文件是否存在且能解析，缺失或格式错误会直接提示并退出。LLM 内容只做解析，不做校验：模型规则不合法、provider 未声明、url 或 api_key 缺失都会在对话时提示，本轮结束但会话继续。可以配置多个 provider，同一个会话里的不同 Agent 按各自模型规则使用不同 provider。运行 `cargo run` 进入对话，输入 `exit`、`quit`、`/exit` 或 Ctrl-D 结束；运行期失败只提示，不会结束进程。

## 工具与沙盒

模型可以调用 `bash` 工具执行命令，命令一律在平台沙盒里运行，不会无沙盒裸跑。

- 边界：整机可读；可写范围只有工作区（启动目录）、系统临时目录与配置里追加的可写根；网络默认关闭。
- 实现：macOS 用系统 Seatbelt（固定 `/usr/bin/sandbox-exec`）；Linux 用 bubblewrap，需要先安装（`sudo apt install bubblewrap` 或 `sudo dnf install bubblewrap`）并允许非特权用户命名空间。
- 沙盒不可用时命令直接失败，并把原因作为工具结果回给模型；不会退回无沙盒执行。
- 命令在独立会话里执行（不接触你的终端），标准输入接 `/dev/null`，超时后整个进程组会被强制终止。
- 终端会把每次调用展示成 `[工具] bash：<命令>`，结果展示成 `[工具结果] …`，方便审计模型做了什么。

`[tools.bash]` 分节的配置项：

- `sandbox_mode`：`workspace-write`（默认，工作区与临时目录可写）或 `read-only`（全部只读）。
- `network_access`：默认 `false`（禁网）。改成 `true` 后命令可以访问网络，例如安装依赖。
- `timeout_ms`：单条命令的默认超时毫秒数，默认 120000，取值范围 1000-600000；模型也能按调用覆盖。
- `writable_roots`：工作区与临时目录之外追加的可写根，写绝对路径（不展开 `~`），例如 `["/Users/you/.cargo"]`。需要 `cargo` 下载依赖时通常要同时打开 `network_access` 并追加 `~/.cargo` 的绝对路径。

## 输出约定

- 正文、思考与输入提示写 stdout，错误提示、会话结束与日志写 stderr。
- 思考块以 `[思考] ` 开头、正文块以 `[助手] ` 开头，前缀只在每块首行出现一次；块切换与回合收尾只换行、不空行。
- 回合结束时若最后一行未换行会自动补上，因此每轮输出都从行首开始、以换行结束；`cargo run > out.txt` 得到的是带前缀的完整对话记录。

## 日志

默认只输出警告与错误，可用 `RUST_LOG` 覆盖，例如 `RUST_LOG=info cargo run`。
