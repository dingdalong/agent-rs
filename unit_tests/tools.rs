use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_openai::types::responses::{InputItem, OutputItem, Tool as LlmTool};
use serde_json::json;

use super::permission::ToolPermissionDecision;
use super::{
    BUILTIN_TOOLS, SandboxFailureEvidence, Tool, ToolBuild, ToolExecutionError, ToolExecutor,
    ToolPermissionJudge, ToolPermissionRequest, select_tools,
};
use crate::{
    config::Config,
    ui::{Ui, UiEvent},
};

/// 回显调用参数的测试工具。
struct Echo {
    executions: Arc<AtomicUsize>,
}

impl Tool for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "回显传入的参数"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
        })
    }

    fn permission_summary(&self, arguments: &str) -> Result<String, String> {
        serde_json::from_str::<serde_json::Value>(arguments)
            .map_err(|error| format!("参数不合法：{error}"))?;
        Ok(self.call_summary(arguments))
    }

    fn call<'a>(
        &'a self,
        arguments: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolExecutionError>> + Send + 'a>> {
        self.executions.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { Ok(format!("回显：{arguments}")) })
    }
}

/// 固定失败的测试工具。
struct Failing;

impl Tool for Failing {
    fn name(&self) -> &'static str {
        "failing"
    }

    fn description(&self) -> &'static str {
        "固定失败"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn call<'a>(
        &'a self,
        _arguments: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolExecutionError>> + Send + 'a>> {
        Box::pin(async move { Err(ToolExecutionError::Other("参数不合法".to_string())) })
    }
}

/// 固定返回沙盒失败的测试工具。
struct SandboxFailing;

impl Tool for SandboxFailing {
    fn name(&self) -> &'static str {
        "sandbox_failing"
    }

    fn description(&self) -> &'static str {
        "固定被沙盒阻止"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn call<'a>(
        &'a self,
        _arguments: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolExecutionError>> + Send + 'a>> {
        Box::pin(async move {
            Err(ToolExecutionError::Sandbox {
                message: "写入工作区外被拒绝\n退出码：1\nstderr：\nOperation not permitted\n"
                    .to_string(),
                evidence: SandboxFailureEvidence::DeniedOutput,
            })
        })
    }
}

/// 记录收到的事件。
#[derive(Default)]
struct RecordingUi {
    events: Vec<UiEvent>,
}

impl Ui for RecordingUi {
    fn read_input(&mut self) -> Option<String> {
        None
    }

    fn render(&mut self, event: UiEvent) {
        self.events.push(event);
    }
}

/// 权限判断替身的预设结论。
#[derive(Clone)]
enum JudgeReply {
    /// 允许执行。
    Allow,
    /// 拒绝执行。
    Deny(&'static str),
    /// 判断本身失败。
    Error(&'static str),
}

/// 固定的权限判断替身，同时记录收到的审查请求。
struct StubJudge {
    reply: JudgeReply,
    requests: Arc<Mutex<Vec<ToolPermissionRequest>>>,
}

impl StubJudge {
    /// 构造一个权限判断替身。
    fn new(reply: JudgeReply) -> Self {
        Self {
            reply,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 记录到的全部审查请求。
    fn requests(&self) -> Vec<ToolPermissionRequest> {
        self.requests
            .lock()
            .expect("权限请求记录锁未被污染")
            .clone()
    }
}

impl ToolPermissionJudge for StubJudge {
    fn judge<'a>(
        &'a self,
        request: ToolPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ToolPermissionDecision, String>> + Send + 'a>> {
        self.requests
            .lock()
            .expect("权限请求记录锁未被污染")
            .push(request);
        let reply = self.reply.clone();
        Box::pin(async move {
            match reply {
                JudgeReply::Allow => Ok(ToolPermissionDecision {
                    allowed: true,
                    reason: "与当前请求相关且未越界".to_string(),
                }),
                JudgeReply::Deny(reason) => Ok(ToolPermissionDecision {
                    allowed: false,
                    reason: reason.to_string(),
                }),
                JudgeReply::Error(message) => Err(message.to_string()),
            }
        })
    }
}

/// 只有 echo 工具的注册表，用例里反复用到。
fn echo_executor() -> (ToolExecutor, Arc<AtomicUsize>) {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor = ToolExecutor::with_tools(vec![Box::new(Echo {
        executions: Arc::clone(&executions),
    })]);
    (executor, executions)
}

/// 构造一条函数调用 item。
fn function_call_item(call_id: &str, name: &str, arguments: &str) -> InputItem {
    let item: OutputItem = serde_json::from_value(json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    }))
    .expect("构造函数调用 item");
    item.into()
}

/// 构造一条助手文本 item。
fn assistant_item(text: &str) -> InputItem {
    let item: OutputItem = serde_json::from_value(json!({
        "id": "item-1",
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [
            {
                "type": "output_text",
                "text": text,
                "annotations": [],
            }
        ],
    }))
    .expect("构造助手 item");
    item.into()
}

/// 提取函数调用结果的 `call_id` 与输出文本；其他 item 返回 `None`。
fn function_call_output(item: &InputItem) -> Option<(String, String)> {
    let value = serde_json::to_value(item).ok()?;
    if value.get("type")?.as_str()? != "function_call_output" {
        return None;
    }
    let call_id = value.get("call_id")?.as_str()?.to_string();
    let output = value.get("output")?.as_str()?.to_string();
    Some((call_id, output))
}

#[tokio::test]
async fn unknown_tool_call_is_reported_with_available_tools() {
    let (executor, _executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[function_call_item("call-1", "run_shell", "{}")],
            "列出工具",
            &judge,
            &mut ui,
        )
        .await;

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "工具「run_shell」不存在。当前可用工具：echo");
    assert!(judge.requests().is_empty());
}

#[tokio::test]
async fn calls_get_one_result_each_in_call_order() {
    let (executor, _executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[
                function_call_item("call-1", "run_shell", "{}"),
                function_call_item("call-2", "read_file", "{}"),
            ],
            "读取两个文件",
            &judge,
            &mut ui,
        )
        .await;

    assert_eq!(results.len(), 2);
    let (first_id, _) = function_call_output(&results[0]).expect("第一条应是函数调用结果");
    let (second_id, _) = function_call_output(&results[1]).expect("第二条应是函数调用结果");
    assert_eq!(first_id, "call-1");
    assert_eq!(second_id, "call-2");
}

#[tokio::test]
async fn registered_tool_result_is_written_back_with_call_id() {
    let (executor, executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[function_call_item("call-1", "echo", "{\"text\":\"你好\"}")],
            "回显你好",
            &judge,
            &mut ui,
        )
        .await;

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "回显：{\"text\":\"你好\"}");
    assert_eq!(executions.load(Ordering::Relaxed), 1);
    let requests = judge.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].user_request, "回显你好");
    assert_eq!(requests[0].tool_name, "echo");
    assert_eq!(requests[0].summary, "{\"text\":\"你好\"}");
}

#[tokio::test]
async fn failing_tool_error_is_written_back_as_result() {
    let executor = ToolExecutor::with_tools(vec![Box::new(Failing)]);
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[function_call_item("call-1", "failing", "{}")],
            "测试失败",
            &judge,
            &mut ui,
        )
        .await;

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "参数不合法");
    assert!(!output.contains("权限判断拒绝"), "{output}");
    assert!(ui.events.contains(&UiEvent::ToolPermission {
        allowed: true,
        reason: "与当前请求相关且未越界".to_string(),
    }));
}

#[tokio::test]
async fn sandbox_failure_is_reported_as_tool_execution_failure_after_permission_allow() {
    let executor = ToolExecutor::with_tools(vec![Box::new(SandboxFailing)]);
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[function_call_item("call-1", "sandbox_failing", "{}")],
            "向工作区外写入测试文件",
            &judge,
            &mut ui,
        )
        .await;

    let (_, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert!(
        output.starts_with("工具「sandbox_failing」执行失败：沙盒拒绝："),
        "{output}"
    );
    assert!(output.contains("Operation not permitted"), "{output}");
    assert!(!output.contains("权限判断拒绝"), "{output}");
    assert_eq!(
        ui.events,
        vec![
            UiEvent::ToolCall {
                name: "sandbox_failing".to_string(),
                summary: "{}".to_string(),
            },
            UiEvent::ToolPermission {
                allowed: true,
                reason: "与当前请求相关且未越界".to_string(),
            },
            UiEvent::ToolResult(output),
        ]
    );
}

#[tokio::test]
async fn non_function_call_items_produce_no_results() {
    let (executor, _executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(&[assistant_item("你好")], "你好", &judge, &mut ui)
        .await;

    assert!(results.is_empty());
    assert!(ui.events.is_empty());
    assert!(judge.requests().is_empty());
}

#[tokio::test]
async fn call_and_result_are_rendered_as_events() {
    let (executor, _executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    executor
        .execute_calls(
            &[function_call_item("call-1", "echo", "{\"text\":\"你好\"}")],
            "回显你好",
            &judge,
            &mut ui,
        )
        .await;

    // 默认摘要就是原始参数；工具可以用 call_summary 覆盖。
    assert_eq!(
        ui.events,
        vec![
            UiEvent::ToolCall {
                name: "echo".to_string(),
                summary: "{\"text\":\"你好\"}".to_string(),
            },
            UiEvent::ToolPermission {
                allowed: true,
                reason: "与当前请求相关且未越界".to_string(),
            },
            UiEvent::ToolResult("回显：{\"text\":\"你好\"}".to_string()),
        ]
    );
}

#[tokio::test]
async fn denied_call_is_not_executed_and_reason_is_returned() {
    let (executor, executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Deny("命令与当前请求无关"));
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[function_call_item("call-1", "echo", "{\"text\":\"你好\"}")],
            "修改报告",
            &judge,
            &mut ui,
        )
        .await;

    assert_eq!(executions.load(Ordering::Relaxed), 0);
    let (_, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert!(output.contains("权限判断拒绝"), "{output}");
    assert!(output.contains("命令与当前请求无关"), "{output}");
    assert!(!output.contains("。。"), "{output}");
    assert_eq!(
        ui.events,
        vec![
            UiEvent::ToolCall {
                name: "echo".to_string(),
                summary: "{\"text\":\"你好\"}".to_string(),
            },
            UiEvent::ToolPermission {
                allowed: false,
                reason: "命令与当前请求无关".to_string(),
            },
        ]
    );
}

#[tokio::test]
async fn judge_error_is_fail_closed() {
    let (executor, executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Error("审查服务不可用"));
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[function_call_item("call-1", "echo", "{\"text\":\"你好\"}")],
            "回显你好",
            &judge,
            &mut ui,
        )
        .await;

    assert_eq!(executions.load(Ordering::Relaxed), 0);
    let (_, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert!(output.contains("失败即拒绝"), "{output}");
    assert!(output.contains("审查服务不可用"), "{output}");
    assert_eq!(
        ui.events,
        vec![
            UiEvent::ToolCall {
                name: "echo".to_string(),
                summary: "{\"text\":\"你好\"}".to_string(),
            },
            UiEvent::ToolPermission {
                allowed: false,
                reason: "智能权限判断失败：审查服务不可用".to_string(),
            },
        ]
    );
}

#[tokio::test]
async fn invalid_arguments_skip_permission_judgment() {
    let (executor, executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[function_call_item("call-1", "echo", "不是 JSON")],
            "回显参数",
            &judge,
            &mut ui,
        )
        .await;

    assert_eq!(executions.load(Ordering::Relaxed), 0);
    assert!(judge.requests().is_empty());
    let (_, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert!(output.contains("参数不合法"), "{output}");
    assert_eq!(ui.events.len(), 2);
    assert!(matches!(ui.events[0], UiEvent::ToolCall { .. }));
    assert!(matches!(ui.events[1], UiEvent::ToolResult(_)));
}

#[tokio::test]
async fn every_registered_call_gets_its_own_judgment() {
    let (executor, executions) = echo_executor();
    let judge = StubJudge::new(JudgeReply::Allow);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[
                function_call_item("call-1", "echo", "{\"text\":\"一\"}"),
                function_call_item("call-2", "echo", "{\"text\":\"二\"}"),
            ],
            "依次回显",
            &judge,
            &mut ui,
        )
        .await;

    assert_eq!(results.len(), 2);
    assert_eq!(executions.load(Ordering::Relaxed), 2);
    let requests = judge.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].summary, "{\"text\":\"一\"}");
    assert_eq!(requests[1].summary, "{\"text\":\"二\"}");
}

#[test]
fn definitions_carry_name_description_and_schema() {
    let (executor, _executions) = echo_executor();

    let definitions = executor.definitions();

    assert_eq!(definitions.len(), 1);
    let value = serde_json::to_value(&definitions[0]).expect("工具定义应可序列化");
    assert_eq!(value["type"], "function");
    assert_eq!(value["name"], "echo");
    assert_eq!(value["description"], "回显传入的参数");
    assert_eq!(value["parameters"]["type"], "object");
    assert!(value["parameters"]["properties"]["text"].is_object());
    // 不启用 strict，避免供应商对严格模式的要求差异。
    assert!(value.get("strict").is_none());
}

/// 注册表测试项：echo 工具的构造函数。
fn build_echo(_config: &Config, _workspace: &Path) -> Box<dyn Tool> {
    Box::new(Echo {
        executions: Arc::new(AtomicUsize::new(0)),
    })
}

/// 注册表测试项：failing 工具的构造函数。
fn build_failing(_config: &Config, _workspace: &Path) -> Box<dyn Tool> {
    Box::new(Failing)
}

/// 提取工具定义里的名称，按注册顺序返回。
fn definition_names(definitions: &[LlmTool]) -> Vec<String> {
    definitions
        .iter()
        .map(|tool| {
            let value = serde_json::to_value(tool).expect("工具定义应可序列化");
            value["name"]
                .as_str()
                .expect("工具定义应有名称")
                .to_string()
        })
        .collect()
}

#[test]
fn builtin_registers_all_tools_when_names_are_absent() {
    let executor = ToolExecutor::builtin(None, &Config::default(), &std::env::temp_dir());

    let expected: Vec<String> = BUILTIN_TOOLS
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    assert!(!expected.is_empty(), "内置工具注册表不应为空");
    assert_eq!(definition_names(&executor.definitions()), expected);
}

#[test]
fn builtin_registers_nothing_for_empty_names() {
    let executor = ToolExecutor::builtin(Some(&[]), &Config::default(), &std::env::temp_dir());

    assert!(executor.definitions().is_empty());
}

#[test]
fn builtin_skips_unknown_names() {
    let executor = ToolExecutor::builtin(
        Some(&["run_shell"]),
        &Config::default(),
        &std::env::temp_dir(),
    );

    assert!(executor.definitions().is_empty());
}

#[test]
fn builtin_registers_duplicate_names_once() {
    let executor = ToolExecutor::builtin(
        Some(&["bash", "bash"]),
        &Config::default(),
        &std::env::temp_dir(),
    );

    assert_eq!(
        definition_names(&executor.definitions()),
        vec!["bash".to_string()]
    );
}

#[test]
fn select_tools_keeps_requested_order_and_skips_unknown_and_duplicate_names() {
    let registry: &[(&str, ToolBuild)] = &[("echo", build_echo), ("failing", build_failing)];

    let tools = select_tools(
        &["failing", "echo", "echo", "run_shell"],
        registry,
        &Config::default(),
        &std::env::temp_dir(),
    );

    let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(names, vec!["failing", "echo"]);
}

#[test]
fn every_registry_entry_builds_a_tool_with_matching_name() {
    let config = Config::default();
    let workspace = std::env::temp_dir();

    for (name, build) in BUILTIN_TOOLS {
        let tool = build(&config, &workspace);
        assert_eq!(tool.name(), *name, "注册表键与工具自报名称必须一致");
    }
}
