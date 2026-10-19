use std::{future::Future, path::Path, pin::Pin};

use async_openai::types::responses::{InputItem, OutputItem, Tool as LlmTool};
use serde_json::json;

use super::{BUILTIN_TOOLS, Tool, ToolBuild, ToolExecutor, select_tools};
use crate::{
    config::Config,
    ui::{Ui, UiEvent},
};

/// 回显调用参数的测试工具。
struct Echo;

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

    fn call<'a>(
        &'a self,
        arguments: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
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
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move { Err("参数不合法".to_string()) })
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

/// 只有 echo 工具的注册表，用例里反复用到。
fn echo_executor() -> ToolExecutor {
    ToolExecutor::with_tools(vec![Box::new(Echo)])
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
    let executor = echo_executor();
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(&[function_call_item("call-1", "run_shell", "{}")], &mut ui)
        .await;

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "工具「run_shell」不存在。当前可用工具：echo");
}

#[tokio::test]
async fn calls_get_one_result_each_in_call_order() {
    let executor = echo_executor();
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[
                function_call_item("call-1", "run_shell", "{}"),
                function_call_item("call-2", "read_file", "{}"),
            ],
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
    let executor = echo_executor();
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(
            &[function_call_item("call-1", "echo", "{\"text\":\"你好\"}")],
            &mut ui,
        )
        .await;

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "回显：{\"text\":\"你好\"}");
}

#[tokio::test]
async fn failing_tool_error_is_written_back_as_result() {
    let executor = ToolExecutor::with_tools(vec![Box::new(Failing)]);
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(&[function_call_item("call-1", "failing", "{}")], &mut ui)
        .await;

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "参数不合法");
}

#[tokio::test]
async fn non_function_call_items_produce_no_results() {
    let executor = echo_executor();
    let mut ui = RecordingUi::default();

    let results = executor
        .execute_calls(&[assistant_item("你好")], &mut ui)
        .await;

    assert!(results.is_empty());
    assert!(ui.events.is_empty());
}

#[tokio::test]
async fn call_and_result_are_rendered_as_events() {
    let executor = echo_executor();
    let mut ui = RecordingUi::default();

    executor
        .execute_calls(
            &[function_call_item("call-1", "echo", "{\"text\":\"你好\"}")],
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
            UiEvent::ToolResult("回显：{\"text\":\"你好\"}".to_string()),
        ]
    );
}

#[test]
fn definitions_carry_name_description_and_schema() {
    let executor = echo_executor();

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
    Box::new(Echo)
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
