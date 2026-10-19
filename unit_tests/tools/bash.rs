use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use serde_json::json;

use super::{
    Bash, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, OUTPUT_BUDGET_BYTES, SANDBOX_HINT,
    SANDBOX_HINT_WITHOUT_NETWORK, effective_timeout_ms, output_budgets, parse_arguments,
    sandbox_hint, truncate_middle,
};
use crate::{
    sandbox::{SandboxMode, SandboxPolicy},
    tools::Tool,
};

/// 每个用例独立的工作区目录，避免相互干扰。
fn temp_workspace(name: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("agent-rs-bash-{}-{name}-{seq}", std::process::id()));
    fs::create_dir_all(&dir).expect("创建临时工作区");
    dir
}

/// 构造一个以临时目录为工作区的 bash 工具。
fn bash_tool(name: &str, network_access: bool) -> Bash {
    let policy = SandboxPolicy::new(
        SandboxMode::WorkspaceWrite,
        network_access,
        temp_workspace(name),
        Vec::new(),
    );
    Bash::new(policy, 120_000)
}

/// 直接调用工具，返回结果文本或错误信息。
async fn call_tool(tool: &Bash, command: &str, timeout_ms: Option<u64>) -> Result<String, String> {
    let mut arguments = json!({ "command": command });
    if let Some(timeout_ms) = timeout_ms {
        arguments["timeout_ms"] = json!(timeout_ms);
    }
    tool.call(arguments.to_string()).await
}

/// 执行命令；沙盒在当前环境不可用（平台缺工具、或测试本身跑在另一个沙盒里）时返回
/// `None`，用例据此跳过。
async fn run(tool: &Bash, command: &str, timeout_ms: Option<u64>) -> Option<String> {
    match call_tool(tool, command, timeout_ms).await {
        Ok(output) if is_environment_block(&output) => {
            eprintln!("跳过：当前环境不允许应用沙盒：{output}");
            None
        }
        Ok(output) => Some(output),
        Err(message) => {
            eprintln!("跳过：沙盒不可用：{message}");
            None
        }
    }
}

/// 嵌套沙盒等环境限制会让 `sandbox-exec` 自己报错，这不算实现失败。
fn is_environment_block(output: &str) -> bool {
    output.contains("sandbox_apply") || output.contains("sandbox-exec:")
}

/// 等待进程消失；用于确认超时后连孙进程一起被回收。
#[cfg(unix)]
async fn wait_until_process_exits(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // 不发信号，只做存在性检查。
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn metadata_follows_the_policy() {
    let offline = bash_tool("metadata-offline", false);
    let online = bash_tool("metadata-online", true);

    assert_eq!(offline.name(), "bash");
    assert!(offline.description().contains("禁止访问网络"));
    assert!(online.description().contains("允许访问网络"));
    let parameters = offline.parameters();
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["required"], json!(["command"]));
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(parameters["properties"]["command"]["type"], "string");
    assert!(
        parameters["properties"]["timeout_ms"]["description"]
            .as_str()
            .expect("超时说明应是文本")
            .contains("120000")
    );
}

#[test]
fn arguments_accept_optional_timeout() {
    let arguments =
        parse_arguments(r#"{"command":"ls","timeout_ms":5000}"#).expect("参数应解析成功");

    assert_eq!(arguments.command, "ls");
    assert_eq!(arguments.timeout_ms, Some(5_000));
}

#[test]
fn arguments_reject_invalid_json() {
    let message = parse_arguments("{").expect_err("非法 JSON 应报错");

    assert!(message.contains("参数不合法"), "{message}");
}

#[test]
fn arguments_reject_unknown_field() {
    let message = parse_arguments(r#"{"command":"ls","cwd":"/tmp"}"#).expect_err("多余字段应报错");

    assert!(message.contains("参数不合法"), "{message}");
}

#[test]
fn arguments_reject_blank_command() {
    let message = parse_arguments(r#"{"command":"   "}"#).expect_err("空命令应报错");

    assert!(message.contains("不能为空"), "{message}");
}

#[test]
fn call_summary_prefers_the_command() {
    let tool = bash_tool("summary", false);

    assert_eq!(tool.call_summary(r#"{"command":"ls -la"}"#), "ls -la");
    // 参数不合法时退回展示原始参数，便于用户看到模型到底发了什么。
    assert_eq!(tool.call_summary("{"), "{");
}

#[test]
fn effective_timeout_is_clamped() {
    assert_eq!(effective_timeout_ms(None, 120_000), 120_000);
    assert_eq!(effective_timeout_ms(Some(10), 120_000), MIN_TIMEOUT_MS);
    assert_eq!(
        effective_timeout_ms(Some(u64::MAX), 120_000),
        MAX_TIMEOUT_MS
    );
    assert_eq!(effective_timeout_ms(Some(5_000), 120_000), 5_000);
}

#[test]
fn output_budgets_reserve_more_for_stderr() {
    let (stdout, stderr) = output_budgets(OUTPUT_BUDGET_BYTES / 2, OUTPUT_BUDGET_BYTES / 2);
    assert_eq!(
        (stdout, stderr),
        (OUTPUT_BUDGET_BYTES / 2, OUTPUT_BUDGET_BYTES / 2)
    );

    let (stdout, stderr) = output_budgets(OUTPUT_BUDGET_BYTES * 4, OUTPUT_BUDGET_BYTES * 4);
    assert_eq!(stdout + stderr, OUTPUT_BUDGET_BYTES);
    assert!(stderr > stdout, "stderr 应拿到更多额度：{stdout}/{stderr}");

    // stderr 很短时，剩余额度回填给 stdout。
    let (stdout, stderr) = output_budgets(OUTPUT_BUDGET_BYTES * 4, 10);
    assert_eq!(stderr, 10);
    assert_eq!(stdout + stderr, OUTPUT_BUDGET_BYTES);
}

#[test]
fn truncate_middle_keeps_head_and_tail() {
    let text = "开头".to_string() + &"x".repeat(600) + "结尾";

    let truncated = truncate_middle(&text, 120);

    assert!(truncated.starts_with("开头"), "{truncated}");
    assert!(truncated.ends_with("结尾"), "{truncated}");
    assert!(truncated.contains("…已省略"), "{truncated}");
    assert!(truncated.len() <= 120 + "…已省略 454 字符…".len());
}

#[test]
fn truncate_middle_keeps_text_within_budget() {
    let text = "a".repeat(100);

    assert_eq!(truncate_middle(&text, 100), text);
    assert_eq!(truncate_middle("短", 10), "短");
}

#[test]
fn sandbox_hint_only_marks_sandbox_failures() {
    let offline = bash_tool("hint-offline", false);
    let online = bash_tool("hint-online", true);

    assert_eq!(
        sandbox_hint(
            &offline.policy,
            "",
            "bash: connect: Operation not permitted"
        ),
        Some(SANDBOX_HINT_WITHOUT_NETWORK)
    );
    assert_eq!(
        sandbox_hint(
            &offline.policy,
            "",
            "curl: (6) Could not resolve host: example.com"
        ),
        Some(SANDBOX_HINT_WITHOUT_NETWORK)
    );
    assert_eq!(sandbox_hint(&offline.policy, "", "全部通过"), None);
    // 允许联网时不再把网络类报错归因于沙盒。
    assert_eq!(
        sandbox_hint(
            &online.policy,
            "",
            "curl: (6) Could not resolve host: example.com"
        ),
        None
    );
    assert_eq!(
        sandbox_hint(&online.policy, "", "touch: Operation not permitted"),
        Some(SANDBOX_HINT)
    );
}

#[tokio::test]
async fn runs_command_in_workspace() {
    let tool = bash_tool("pwd", false);
    let Some(output) = run(&tool, "pwd", None).await else {
        return;
    };

    assert!(output.contains("退出码：0"), "{output}");
    assert!(output.contains("耗时："), "{output}");
    assert!(output.contains("stdout："), "{output}");
    let workspace = fs::canonicalize(&tool.policy.workspace).expect("解析工作区");
    assert!(
        output.contains(&workspace.display().to_string()),
        "{output}"
    );
}

#[tokio::test]
async fn captures_stderr_and_exit_code() {
    let tool = bash_tool("stderr", false);
    let Some(output) = run(&tool, "printf 报错 >&2; exit 3", None).await else {
        return;
    };

    assert!(output.contains("退出码：3"), "{output}");
    assert!(output.contains("stderr："), "{output}");
    assert!(output.contains("报错"), "{output}");
}

#[tokio::test]
async fn empty_output_is_reported() {
    let tool = bash_tool("empty", false);
    let Some(output) = run(&tool, "true", None).await else {
        return;
    };

    assert!(output.contains("退出码：0"), "{output}");
    assert!(output.contains("（无输出）"), "{output}");
}

#[tokio::test]
async fn stdin_is_closed_so_commands_do_not_wait_for_input() {
    let tool = bash_tool("stdin", false);
    let Some(output) = run(&tool, "cat", Some(1_000)).await else {
        return;
    };

    assert!(output.contains("退出码：0"), "{output}");
    assert!(!output.contains("超时"), "{output}");
}

#[tokio::test]
async fn writes_inside_workspace_and_denies_outside_writes() {
    let tool = bash_tool("write", false);
    let Some(output) = run(&tool, "printf hi > inside.txt", None).await else {
        return;
    };
    assert!(output.contains("退出码：0"), "{output}");
    let inside = tool.policy.workspace.join("inside.txt");
    assert_eq!(
        fs::read_to_string(&inside).expect("工作区内应写入成功"),
        "hi"
    );

    let outside_dir = fs::canonicalize("/var/tmp").expect("系统应有 /var/tmp");
    let outside = outside_dir.join(format!("agent-rs-outside-{}.txt", std::process::id()));
    let command = format!("printf hi > {}", outside.display());
    let Some(output) = run(&tool, &command, None).await else {
        return;
    };
    assert!(!output.contains("退出码：0"), "{output}");
    assert!(!outside.exists(), "工作区外不应被写入：{output}");
}

#[tokio::test]
async fn denies_network_when_policy_disables_it() {
    let tool = bash_tool("network-off", false);
    let Some(output) = run(&tool, "exec 3<>/dev/tcp/127.0.0.1/1", None).await else {
        return;
    };

    // 连接被沙盒拒绝：命令失败，并在结果里给出边界提示。
    assert!(!output.contains("退出码：0"), "{output}");
    assert!(output.contains("Operation not permitted"), "{output}");
    assert!(
        output.contains("提示：该失败可能由沙盒限制导致"),
        "{output}"
    );
}

#[tokio::test]
async fn allows_loopback_when_policy_enables_network() {
    let tool = bash_tool("network-on", true);
    let Some(output) = run(&tool, "exec 3<>/dev/tcp/127.0.0.1/1", None).await else {
        return;
    };

    // 没有服务监听这个端口，能走到「连接被拒绝」就说明网络没有被沙盒拦掉。
    assert!(output.contains("Connection refused"), "{output}");
}

#[tokio::test]
async fn sandbox_marker_is_visible_to_commands() {
    let tool = bash_tool("marker", false);
    let expected = crate::sandbox::kind().to_string();
    let Some(output) = run(&tool, "printf %s \"$AGENT_SANDBOX\"", None).await else {
        return;
    };

    assert!(output.contains(&expected), "{output}");
}

/// 策略不能挡住日常开发命令：以仓库根为工作区跑一次 git 与 cargo。
#[tokio::test]
async fn development_commands_run_inside_the_sandbox() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("跳过：本机没有 git");
        return;
    }
    let workspace = std::env::current_dir().expect("获取当前目录");
    let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, false, workspace, Vec::new());
    let tool = Bash::new(policy, 120_000);
    let Some(output) = run(
        &tool,
        "git rev-parse --is-inside-work-tree && cargo --version",
        Some(60_000),
    )
    .await
    else {
        return;
    };

    assert!(output.contains("退出码：0"), "{output}");
    assert!(output.contains("true"), "{output}");
    assert!(output.contains("cargo"), "{output}");
}

#[tokio::test]
async fn timeout_kills_the_whole_process_group() {
    let tool = bash_tool("timeout", false);
    let Some(output) = run(&tool, "sleep 30 & echo $! > pid.txt; wait", Some(1_000)).await else {
        return;
    };

    assert!(output.contains("超时"), "{output}");
    let pid: i32 = fs::read_to_string(tool.policy.workspace.join("pid.txt"))
        .expect("命令应记录后台进程号")
        .trim()
        .parse()
        .expect("进程号应是整数");
    assert!(
        wait_until_process_exits(pid).await,
        "超时后后台进程 {pid} 应被一并终止"
    );
}
