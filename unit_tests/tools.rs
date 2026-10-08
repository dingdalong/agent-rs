use async_openai::types::responses::{InputItem, OutputItem};
use serde_json::json;

use super::{Tool, ToolExecutor};

/// 回显调用参数的测试工具。
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn call(&self, arguments: &str) -> Result<String, String> {
        Ok(format!("回显：{arguments}"))
    }
}

/// 固定失败的测试工具。
struct FailingTool;

impl Tool for FailingTool {
    fn name(&self) -> &'static str {
        "failing"
    }

    fn call(&self, _arguments: &str) -> Result<String, String> {
        Err("参数不合法".to_string())
    }
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

#[test]
fn unknown_tool_call_is_reported_with_available_tools() {
    let executor = ToolExecutor::new();

    let results = executor.execute_calls(&[function_call_item("call-1", "run_shell", "{}")]);

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "工具「run_shell」不存在。当前可用工具：无");
}

#[test]
fn unknown_tool_message_lists_registered_names() {
    let executor = ToolExecutor::with_tools(vec![Box::new(EchoTool)]);

    let results = executor.execute_calls(&[function_call_item("call-2", "run_shell", "{}")]);

    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-2");
    assert_eq!(output, "工具「run_shell」不存在。当前可用工具：echo");
}

#[test]
fn calls_get_one_result_each_in_call_order() {
    let executor = ToolExecutor::new();

    let results = executor.execute_calls(&[
        function_call_item("call-1", "run_shell", "{}"),
        function_call_item("call-2", "read_file", "{}"),
    ]);

    assert_eq!(results.len(), 2);
    let (first_id, _) = function_call_output(&results[0]).expect("第一条应是函数调用结果");
    let (second_id, _) = function_call_output(&results[1]).expect("第二条应是函数调用结果");
    assert_eq!(first_id, "call-1");
    assert_eq!(second_id, "call-2");
}

#[test]
fn registered_tool_result_is_written_back_with_call_id() {
    let executor = ToolExecutor::with_tools(vec![Box::new(EchoTool)]);

    let results =
        executor.execute_calls(&[function_call_item("call-1", "echo", "{\"text\":\"你好\"}")]);

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "回显：{\"text\":\"你好\"}");
}

#[test]
fn failing_tool_error_is_written_back_as_result() {
    let executor = ToolExecutor::with_tools(vec![Box::new(FailingTool)]);

    let results = executor.execute_calls(&[function_call_item("call-1", "failing", "{}")]);

    assert_eq!(results.len(), 1);
    let (call_id, output) = function_call_output(&results[0]).expect("应产出函数调用结果");
    assert_eq!(call_id, "call-1");
    assert_eq!(output, "参数不合法");
}

#[test]
fn non_function_call_items_produce_no_results() {
    let executor = ToolExecutor::new();

    let results = executor.execute_calls(&[assistant_item("你好")]);

    assert!(results.is_empty());
}
