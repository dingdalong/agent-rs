//! bash 工具：把模型给的一条命令放进沙盒执行，负责超时、进程清理与结果整理。
//!
//! - 标准输入接 `/dev/null`，避免命令停在等输入上；
//! - 命令放进独立会话与进程组，既碰不到用户的终端，超时也能整组终止；
//! - 超时先 `SIGKILL` 整个进程组再回收；
//! - 输出按 stdout 1/3、stderr 2/3 分配预算，超长在中间截断；
//! - 命中沙盒拒绝特征时补一行提示，让模型知道这是边界而不是命令写错了。
//!
//! 沙盒不可用时直接返回错误（fail-closed），不会退回无沙盒执行。

use std::{
    fmt::Write as _,
    future::Future,
    path::Path,
    pin::Pin,
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};

#[cfg(unix)]
use nix::{errno::Errno, sys::signal::Signal, unistd::Pid};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinHandle,
    time::timeout,
};

use crate::{
    config::Config,
    sandbox::{self, SandboxMode, SandboxPolicy},
    tools::{SandboxFailureEvidence, Tool, ToolExecutionError},
};

/// 单次调用的最短超时。
const MIN_TIMEOUT_MS: u64 = 1_000;
/// 单次调用的最长超时（10 分钟）。
const MAX_TIMEOUT_MS: u64 = 600_000;
/// 每次调用保留的输出总量。
const OUTPUT_BUDGET_BYTES: usize = 32 * 1024;
/// 每路管道最多收集的字节数；超出后继续读取但不再保留，避免命令刷爆内存。
const STREAM_CAP_BYTES: usize = 1024 * 1024;
/// 超时后等待输出读取收尾的时间。
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// 沙盒拒绝的典型报错特征。
const DENIED_KEYWORDS: [&str; 5] = [
    "operation not permitted",
    "permission denied",
    "read-only file system",
    "seccomp",
    "landlock",
];

/// 禁网时常见的网络类失败特征。
const NETWORK_DENIED_KEYWORDS: [&str; 4] = [
    "could not resolve host",
    "temporary failure in name resolution",
    "network is unreachable",
    "name or service not known",
];

/// 沙盒限制导致的失败提示（允许联网版本）。
const SANDBOX_HINT: &str =
    "（提示：该失败可能由沙盒限制导致——只有工作区与临时目录可写，系统其余部分只读。）";
/// 沙盒限制导致的失败提示（禁止联网版本）。
const SANDBOX_HINT_WITHOUT_NETWORK: &str =
    "（提示：该失败可能由沙盒限制导致——只有工作区与临时目录可写，且当前策略禁止联网。）";

/// bash 工具：按沙盒策略执行命令，并把结果整理成模型可读的文本。
pub struct Bash {
    /// 命令运行的沙盒策略，工作目录也取自这里。
    policy: SandboxPolicy,
    /// 配置的默认超时（毫秒）。
    timeout_ms: u64,
    /// 启动时的沙盒探测结果；失败时命令一律拒绝执行。
    probe: Result<(), String>,
    /// 工具说明，按当前策略生成。
    description: String,
    /// 参数 Schema，按当前默认超时生成。
    parameters: serde_json::Value,
}

impl Bash {
    /// 注册名：注册表与模型都用这个名称。
    pub const NAME: &str = "bash";

    /// 用沙盒策略与默认超时构造工具，并做一次沙盒可用性自检。
    pub fn new(policy: SandboxPolicy, timeout_ms: u64) -> Self {
        let clamped = timeout_ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        if clamped != timeout_ms {
            tracing::warn!(
                configured = timeout_ms,
                used = clamped,
                "配置的命令超时超出允许范围，已按范围取值"
            );
        }
        let timeout_ms = clamped;
        let probe = policy.probe();
        match &probe {
            Ok(()) => tracing::info!(
                sandbox = sandbox::kind(),
                workspace = %policy.workspace.display(),
                writable_roots = ?policy.effective_writable_roots(),
                network_access = policy.network_access,
                timeout_ms,
                "bash 工具初始化完成"
            ),
            Err(message) => tracing::error!(
                sandbox = sandbox::kind(),
                %message,
                "bash 工具初始化完成，但沙盒不可用，命令将被拒绝"
            ),
        }
        let description = build_description(&policy);
        let parameters = build_parameters(timeout_ms);
        Self {
            policy,
            timeout_ms,
            probe,
            description,
            parameters,
        }
    }

    /// 按工具配置与工作目录装配工具，作为内置工具注册表的构造入口。
    pub fn build(config: &Config, workspace: &Path) -> Box<dyn Tool> {
        let policy = SandboxPolicy::new(
            config.bash.sandbox_mode,
            config.bash.network_access,
            workspace.to_path_buf(),
            config.bash.writable_roots.clone(),
        );
        Box::new(Self::new(policy, config.bash.timeout_ms))
    }

    /// 执行一条命令：沙盒问题返回结构化错误，命令自身失败保留在结果文本里。
    async fn run(&self, arguments: &Arguments) -> Result<String, ToolExecutionError> {
        if let Err(message) = &self.probe {
            return Err(ToolExecutionError::Sandbox {
                message: message.clone(),
                evidence: SandboxFailureEvidence::Unavailable,
            });
        }
        let timeout_ms = effective_timeout_ms(arguments.timeout_ms, self.timeout_ms);
        let mut command = self
            .policy
            .wrap(&arguments.command, &self.policy.workspace)
            .map_err(|message| ToolExecutionError::Sandbox {
                message,
                evidence: SandboxFailureEvidence::SetupFailed,
            })?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        set_process_isolation(&mut command);

        let started = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|error| ToolExecutionError::Sandbox {
                message: format!("启动沙盒命令失败：{error}"),
                evidence: SandboxFailureEvidence::SetupFailed,
            })?;
        let pid = child.id().ok_or_else(|| ToolExecutionError::Sandbox {
            message: "启动沙盒命令失败：拿不到子进程号".to_string(),
            evidence: SandboxFailureEvidence::SetupFailed,
        })?;
        let stdout = child.stdout.take().expect("标准输出已设置为管道");
        let stderr = child.stderr.take().expect("标准错误已设置为管道");
        let stdout_task = tokio::spawn(read_stream(stdout));
        let stderr_task = tokio::spawn(read_stream(stderr));

        let status =
            if let Ok(result) = timeout(Duration::from_millis(timeout_ms), child.wait()).await {
                Some(result.map_err(|error| {
                    ToolExecutionError::Other(format!("等待命令结束失败：{error}"))
                })?)
            } else {
                // 超时：整组终止后回收，避免留下后台进程。
                kill_process_group(pid);
                let _ = child.start_kill();
                let _ = child.wait().await;
                None
            };
        let stdout = collect_output(stdout_task).await;
        let stderr = collect_output(stderr_task).await;
        if status.is_none() {
            tracing::warn!(timeout_ms, "bash 命令超时，已强制终止");
        }

        let parts = ResultParts {
            timed_out: status.is_none(),
            timeout_ms,
            status,
            duration: started.elapsed(),
            stdout: &stdout,
            stderr: &stderr,
        };
        let output = format_result(&parts);
        let stdout_text = String::from_utf8_lossy(&stdout);
        let stderr_text = String::from_utf8_lossy(&stderr);
        if let Some(hint) = sandbox_hint(&self.policy, &stdout_text, &stderr_text) {
            return Err(ToolExecutionError::Sandbox {
                message: format!("{output}{hint}\n"),
                evidence: SandboxFailureEvidence::DeniedOutput,
            });
        }
        Ok(output)
    }
}

impl Tool for Bash {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    fn call_summary(&self, arguments: &str) -> String {
        parse_arguments(arguments)
            .map_or_else(|_| arguments.to_string(), |arguments| arguments.command)
    }

    fn permission_summary(&self, arguments: &str) -> Result<String, String> {
        parse_arguments(arguments).map(|arguments| arguments.command)
    }

    fn call<'a>(
        &'a self,
        arguments: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolExecutionError>> + Send + 'a>> {
        Box::pin(async move {
            let arguments = parse_arguments(&arguments).map_err(ToolExecutionError::Other)?;
            self.run(&arguments).await
        })
    }
}

/// bash 工具的参数：一条命令与可选超时。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    /// 要执行的 bash 命令。
    command: String,
    /// 覆盖默认超时的毫秒数。
    timeout_ms: Option<u64>,
}

/// 解析模型给出的参数；结构不对或命令为空时返回面向模型的说明。
fn parse_arguments(arguments: &str) -> Result<Arguments, String> {
    let arguments: Arguments = serde_json::from_str(arguments).map_err(|error| {
        format!(
            "bash 参数不合法：{error}。期望 JSON：{{\"command\": \"要执行的命令\", \"timeout_ms\": 可选毫秒数}}"
        )
    })?;
    if arguments.command.trim().is_empty() {
        return Err("bash 参数不合法：command 不能为空".to_string());
    }
    Ok(arguments)
}

/// 单次调用的有效超时：模型可以覆盖，但会被钳制在允许范围内。
fn effective_timeout_ms(requested: Option<u64>, default_ms: u64) -> u64 {
    requested
        .unwrap_or(default_ms)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
}

/// 按当前策略生成工具说明：模型据此知道工作目录、可写范围与网络边界。
fn build_description(policy: &SandboxPolicy) -> String {
    let mut description = format!(
        "在工作区沙盒里执行一条 bash 命令，返回退出码、耗时与输出。工作目录：{}。",
        policy.workspace.display()
    );
    match policy.mode {
        SandboxMode::WorkspaceWrite => {
            let roots = policy
                .effective_writable_roots()
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join("、");
            let _ = write!(
                description,
                "沙盒模式：workspace-write；实际可写根：{roots}；系统其余部分只读；"
            );
        }
        SandboxMode::ReadOnly => {
            description.push_str("沙盒模式：read-only；任何目录都不可写；");
        }
    }
    if policy.network_access {
        description.push_str("允许访问网络。");
    } else {
        description.push_str("禁止访问网络，联网命令会失败。");
    }
    description.push_str("不要使用 sudo；命令超过超时时间会被强制终止。");
    description
}

/// 参数 Schema：命令必填，超时可选。
fn build_parameters(timeout_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "要执行的 bash 命令，例如 `cargo test`",
            },
            "timeout_ms": {
                "type": "integer",
                "description": format!(
                    "超时毫秒数，默认 {timeout_ms}，取值范围 {MIN_TIMEOUT_MS}-{MAX_TIMEOUT_MS}"
                ),
            },
        },
        "required": ["command"],
        "additionalProperties": false,
    })
}

/// 让命令脱离终端并独立成组：既碰不到用户的终端，超时也能整组终止。
#[cfg(unix)]
#[allow(unsafe_code)]
fn set_process_isolation(command: &mut Command) {
    #[cfg(target_os = "linux")]
    let parent_pid = nix::unistd::getpid();
    // SAFETY: pre_exec 在 fork 之后、exec 之前调用，这里只调用 nix 薄封装的异步信号安全
    // 系统调用，不分配内存、不加锁，失败会终止 spawn 而不是带着半成品状态继续。
    unsafe {
        command.pre_exec(move || {
            detach_from_tty()?;
            #[cfg(target_os = "linux")]
            set_parent_death_signal(parent_pid)?;
            Ok(())
        });
    }
}

/// 非 Unix 平台没有「会话/进程组」概念，保持原样。
#[cfg(not(unix))]
fn set_process_isolation(_command: &mut Command) {}

/// 新建会话以脱离控制终端；已经是进程组组长时退回「独立进程组」。
#[cfg(unix)]
fn detach_from_tty() -> std::io::Result<()> {
    match nix::unistd::setsid() {
        Ok(_) => Ok(()),
        Err(Errno::EPERM) => set_process_group(),
        Err(errno) => Err(errno.into()),
    }
}

/// 把当前进程放进新的进程组。
#[cfg(unix)]
fn set_process_group() -> std::io::Result<()> {
    // 参数 0 表示调用进程自身。
    nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(std::io::Error::from)
}

/// 父进程退出时让子进程收到 `SIGTERM`，避免宿主被强杀后留下孤儿命令。
#[cfg(target_os = "linux")]
fn set_parent_death_signal(parent_pid: Pid) -> std::io::Result<()> {
    nix::sys::prctl::set_pdeathsig(Some(Signal::SIGTERM)).map_err(std::io::Error::from)?;
    // 父进程可能在 fork 与 pre_exec 之间退出，这里补一次检查。
    if nix::unistd::getppid() != parent_pid {
        let _ = nix::sys::signal::raise(Signal::SIGTERM);
    }
    Ok(())
}

/// 向命令所在的进程组发 `SIGKILL`（尽力而为）。
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // pid 来自刚启动的子进程，不会超出 pid_t 的范围。
    let process_group_id = Pid::from_raw(pid.cast_signed());
    if let Err(errno) = nix::sys::signal::killpg(process_group_id, Signal::SIGKILL) {
        let error = std::io::Error::from(errno);
        tracing::warn!(%error, pid, "终止命令进程组失败");
    }
}

/// 非 Unix 平台无法按进程组终止，交给 tokio 的 `kill_on_drop`。
#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// 读取一路管道：最多保留 `STREAM_CAP_BYTES`，超出后继续读取但丢弃。
async fn read_stream(mut stream: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(count) => {
                let remaining = STREAM_CAP_BYTES.saturating_sub(buffer.len());
                buffer.extend_from_slice(&chunk[..count.min(remaining)]);
            }
            Err(error) => {
                tracing::warn!(%error, "读取命令输出失败");
                break;
            }
        }
    }
    buffer
}

/// 收集输出读取任务的结果；收尾超时或任务失败时按空输出处理，不拖住整轮对话。
async fn collect_output(mut handle: JoinHandle<Vec<u8>>) -> Vec<u8> {
    match timeout(CLEANUP_TIMEOUT, &mut handle).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            tracing::warn!(%error, "命令输出读取任务失败");
            Vec::new()
        }
        Err(_) => {
            handle.abort();
            tracing::warn!("命令输出读取超时，已放弃剩余输出");
            Vec::new()
        }
    }
}

/// 整理结果文本所需的原始信息。
struct ResultParts<'a> {
    /// 是否因超时被强制终止。
    timed_out: bool,
    /// 本次调用的有效超时。
    timeout_ms: u64,
    /// 命令退出状态；超时时为 `None`。
    status: Option<ExitStatus>,
    /// 命令执行耗时。
    duration: Duration,
    /// 标准输出原始字节。
    stdout: &'a [u8],
    /// 标准错误原始字节。
    stderr: &'a [u8],
}

/// 把执行结果整理成模型可读的文本：元信息在前，输出在后。
fn format_result(parts: &ResultParts<'_>) -> String {
    let mut text = String::new();
    if parts.timed_out {
        let _ = writeln!(text, "命令在 {} 毫秒后超时，已强制终止。", parts.timeout_ms);
    } else if let Some(status) = &parts.status {
        text.push_str(&exit_line(*status));
        text.push('\n');
    }
    let _ = writeln!(text, "耗时：{:.1} 秒\n输出：", parts.duration.as_secs_f32());

    let (stdout_budget, stderr_budget) = output_budgets(parts.stdout.len(), parts.stderr.len());
    let stdout = String::from_utf8_lossy(parts.stdout);
    let stderr = String::from_utf8_lossy(parts.stderr);
    if parts.stdout.is_empty() && parts.stderr.is_empty() {
        text.push_str("（无输出）\n");
    }
    if !parts.stdout.is_empty() {
        text.push_str("stdout：\n");
        push_section(&mut text, &truncate_middle(&stdout, stdout_budget));
    }
    if !parts.stderr.is_empty() {
        text.push_str("stderr：\n");
        push_section(&mut text, &truncate_middle(&stderr, stderr_budget));
    }

    text
}

/// 追加一段输出，保证它以换行结尾，便于后面继续拼接。
fn push_section(text: &mut String, section: &str) {
    text.push_str(section);
    if !section.ends_with('\n') {
        text.push('\n');
    }
}

/// 退出码一行；被信号终止时说明信号号。
fn exit_line(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("退出码：{code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return format!("退出码：无（被信号 {signal} 终止）");
        }
    }
    "退出码：未知".to_string()
}

/// 输出预算：总量没有超限就各自全留；超限时 stdout 保 1/3、stderr 保 2/3，
/// stderr 用不完的额度回填给 stdout。
fn output_budgets(stdout_len: usize, stderr_len: usize) -> (usize, usize) {
    if stdout_len + stderr_len <= OUTPUT_BUDGET_BYTES {
        return (stdout_len, stderr_len);
    }
    let stdout_budget = stdout_len.min(OUTPUT_BUDGET_BYTES / 3);
    let stderr_budget = stderr_len.min(OUTPUT_BUDGET_BYTES - stdout_budget);
    let remaining = OUTPUT_BUDGET_BYTES - stdout_budget - stderr_budget;
    let stdout_budget = stdout_budget + remaining.min(stdout_len - stdout_budget);
    (stdout_budget, stderr_budget)
}

/// 中间截断：保留头部 2/3 与尾部 1/3，中间用省略标记替代。
///
/// `budget` 小于文本长度时，头尾预算之和等于 `budget`，因此两段不会重叠。
fn truncate_middle(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let head_budget = budget * 2 / 3;
    let tail_budget = budget - head_budget;
    let head_end = floor_char_boundary(text, head_budget);
    let tail_start = ceil_char_boundary(text, text.len() - tail_budget);
    let omitted = text[head_end..tail_start].chars().count();
    format!(
        "{}…已省略 {omitted} 字符…{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// 找到不大于 `max` 的最后一个字符边界。
fn floor_char_boundary(text: &str, max: usize) -> usize {
    let mut index = max.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 找到不小于 `min` 的第一个字符边界。
fn ceil_char_boundary(text: &str, min: usize) -> usize {
    let mut index = min.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// 命中沙盒拒绝特征时给出提示；否则返回 `None`。
fn sandbox_hint(policy: &SandboxPolicy, stdout: &str, stderr: &str) -> Option<&'static str> {
    let mut keywords: Vec<&str> = DENIED_KEYWORDS.to_vec();
    if !policy.network_access {
        keywords.extend(NETWORK_DENIED_KEYWORDS);
    }
    let matched = [stdout, stderr].iter().any(|section| {
        let lowered = section.to_lowercase();
        keywords
            .iter()
            .any(|keyword| lowered.contains(&keyword.to_lowercase()))
    });
    if !matched {
        return None;
    }
    Some(if policy.network_access {
        SANDBOX_HINT
    } else {
        SANDBOX_HINT_WITHOUT_NETWORK
    })
}

#[cfg(test)]
#[path = "../../unit_tests/tools/bash.rs"]
mod tests;
