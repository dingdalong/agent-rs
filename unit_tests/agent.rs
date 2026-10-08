use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use async_openai::types::responses::{InputItem, OutputItem};
use serde_json::json;

use super::Agent;
use crate::{
    config::Config,
    llm::{LLM, StepOutcome},
    ui::{Ui, UiEvent},
};

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

/// 丢弃所有事件的 UI：渲染失败由实现自行消化，不影响会话。
struct SilentUi;

impl Ui for SilentUi {
    fn read_input(&mut self) -> Option<String> {
        None
    }

    fn render(&mut self, _event: UiEvent) {}
}

/// 测试替身：按预设返回结果，并记录收到的请求。
struct StubLlm {
    replies: Mutex<VecDeque<StubReply>>,
    state: Arc<StubState>,
}

/// 替身共享状态，供用例在请求发出后断言。
#[derive(Default)]
struct StubState {
    requests: Mutex<Vec<Vec<InputItem>>>,
}

impl StubState {
    /// 最近一次收到的请求 items。
    fn last_request(&self) -> Vec<InputItem> {
        self.requests
            .lock()
            .expect("请求记录锁未被污染")
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

/// 用例期望的替身行为。
#[derive(Clone)]
enum StubReply {
    /// 正常返回一段助手文本。
    Answer(&'static str),
    /// 返回一个函数调用步骤；字符串是模型给出的工具名。
    FunctionCall(&'static str),
    /// 返回一个包含多个函数调用的步骤；(`call_id`, 工具名) 按调用顺序排列。
    FunctionCalls(Vec<(&'static str, &'static str)>),
    /// 返回模型侧错误。
    ModelError(&'static str),
}

impl LLM for StubLlm {
    fn chat(
        &self,
        input: Vec<InputItem>,
        _ui: &mut (dyn Ui + Send),
    ) -> Pin<Box<dyn Future<Output = Result<StepOutcome, String>> + Send + '_>> {
        self.state
            .requests
            .lock()
            .expect("请求记录锁未被污染")
            .push(input);
        let reply = self
            .replies
            .lock()
            .expect("回复队列锁未被污染")
            .pop_front()
            .expect("替身预设的回复已用完");
        Box::pin(async move {
            match reply {
                StubReply::Answer(text) => Ok(StepOutcome {
                    output_items: vec![output_item(text)],
                    text: text.to_string(),
                }),
                StubReply::FunctionCall(name) => Ok(StepOutcome {
                    output_items: vec![function_call_item("call-1", name, "{}")],
                    text: String::new(),
                }),
                StubReply::FunctionCalls(calls) => Ok(StepOutcome {
                    output_items: calls
                        .iter()
                        .map(|(call_id, name)| function_call_item(call_id, name, "{}"))
                        .collect(),
                    text: String::new(),
                }),
                StubReply::ModelError(message) => Err(message.to_string()),
            }
        })
    }
}

/// 构造一条助手输出 item。
fn output_item(text: &str) -> InputItem {
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
    .expect("构造输出 item");
    item.into()
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

/// 提取函数调用的工具名；非函数调用 item 返回 `None`。
fn function_call_name(item: &InputItem) -> Option<String> {
    let value = serde_json::to_value(item).ok()?;
    if value.get("type")?.as_str()? != "function_call" {
        return None;
    }
    value.get("name")?.as_str().map(str::to_string)
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

/// 提取 user message 的文本；非 user message 返回 `None`。
fn user_text(item: &InputItem) -> Option<String> {
    let value = serde_json::to_value(item).ok()?;
    if value.get("role")?.as_str()? != "user" {
        return None;
    }
    value.get("content")?.as_str().map(str::to_string)
}

/// 注入替身的 Agent，不发起真实请求。
fn agent_with(reply: StubReply) -> Agent {
    agent_with_replies(vec![reply]).0
}

/// 注入多步替身回复的 Agent，并返回共享状态供请求断言。
fn agent_with_replies(replies: Vec<StubReply>) -> (Agent, Arc<StubState>) {
    let state = Arc::new(StubState::default());
    let mut agent = Agent::new(Config::default()).with_model("test/model");
    agent.llm = Some(stub_llm(replies, Arc::clone(&state)));
    (agent, state)
}

/// 构造按序返回给定回复的替身。
fn stub_llm(replies: Vec<StubReply>, state: Arc<StubState>) -> Box<dyn LLM> {
    Box::new(StubLlm {
        replies: Mutex::new(replies.into()),
        state,
    })
}

#[tokio::test]
async fn chat_records_history_on_success() {
    let mut agent = agent_with(StubReply::Answer("你好，我是助手。"));
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    assert_eq!(ui.events, vec![UiEvent::TurnFinished]);
    assert_eq!(agent.history.len(), 2);
    assert!(matches!(agent.history[1], InputItem::Item(_)));
}

#[tokio::test]
async fn chat_requests_history_plus_current_input() {
    let (mut agent, state) =
        agent_with_replies(vec![StubReply::Answer("回答"), StubReply::Answer("回答")]);
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;
    agent.chat("再来", &mut ui).await;

    let requests = state.requests.lock().expect("请求记录锁未被污染");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].len(), 1);
    assert_eq!(user_text(&requests[0][0]).as_deref(), Some("你好"));
    // 第二次请求 = 已提交历史（首轮输入 + 输出）+ 本轮输入。
    assert_eq!(requests[1].len(), 3);
    assert_eq!(user_text(&requests[1][0]).as_deref(), Some("你好"));
    assert!(matches!(requests[1][1], InputItem::Item(_)));
    assert_eq!(user_text(&requests[1][2]).as_deref(), Some("再来"));
}

#[tokio::test]
async fn chat_renders_model_error_without_touching_history() {
    let mut agent = agent_with(StubReply::ModelError("服务端错误"));
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    assert_eq!(ui.events, vec![UiEvent::Error("服务端错误".to_string())]);
    assert!(agent.history.is_empty());
}

#[tokio::test]
async fn silent_ui_does_not_stop_failed_turn() {
    let mut agent = agent_with(StubReply::ModelError("服务端错误"));
    let mut ui = SilentUi;

    agent.chat("你好", &mut ui).await;

    assert!(agent.history.is_empty());
}

#[tokio::test]
async fn failed_turn_does_not_block_next_turn() {
    let mut agent = agent_with(StubReply::ModelError("服务端错误"));
    let mut ui = RecordingUi::default();
    agent.chat("你好", &mut ui).await;
    assert!(agent.history.is_empty());

    let state = Arc::new(StubState::default());
    agent.llm = Some(stub_llm(
        vec![StubReply::Answer("恢复后的回答")],
        Arc::clone(&state),
    ));
    let mut ui = RecordingUi::default();
    agent.chat("再来一次", &mut ui).await;

    assert_eq!(ui.events, vec![UiEvent::TurnFinished]);
    assert_eq!(agent.history.len(), 2);
    // 失败轮不留下任何输入：重试请求只包含本轮 user message。
    let request = state.last_request();
    assert_eq!(request.len(), 1);
    assert_eq!(user_text(&request[0]).as_deref(), Some("再来一次"));
}

#[tokio::test]
async fn unknown_tool_call_result_is_committed_and_turn_continues() {
    let (mut agent, state) = agent_with_replies(vec![
        StubReply::FunctionCall("run_shell"),
        StubReply::Answer("我换一种方式完成。"),
    ]);
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    // 工具不存在不是用户可见错误：结果回给模型，由模型继续本轮。
    assert_eq!(ui.events, vec![UiEvent::TurnFinished]);
    // 历史：用户输入 + 函数调用 + 工具结果 + 模型最终回答。
    assert_eq!(agent.history.len(), 4);
    assert_eq!(user_text(&agent.history[0]).as_deref(), Some("你好"));
    assert_eq!(
        function_call_name(&agent.history[1]).as_deref(),
        Some("run_shell")
    );
    let (call_id, output) = function_call_output(&agent.history[2]).expect("第三步应是工具结果");
    assert_eq!(call_id, "call-1");
    assert!(output.contains("run_shell"), "{output}");
    assert!(output.contains("不存在"), "{output}");
    assert!(matches!(agent.history[3], InputItem::Item(_)));

    // 第二个请求 = 已提交历史（含调用与结果），本轮输入不再重复。
    let requests = state.requests.lock().expect("请求记录锁未被污染");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].len(), 3);
    assert_eq!(
        function_call_name(&requests[1][1]).as_deref(),
        Some("run_shell")
    );
    assert!(function_call_output(&requests[1][2]).is_some());
}

#[tokio::test]
async fn multiple_unknown_tool_calls_get_results_in_order() {
    let (mut agent, _state) = agent_with_replies(vec![
        StubReply::FunctionCalls(vec![("call-1", "run_shell"), ("call-2", "read_file")]),
        StubReply::Answer("好的。"),
    ]);
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    assert_eq!(ui.events, vec![UiEvent::TurnFinished]);
    // 历史：用户输入 + 两次调用 + 两条结果 + 最终回答。
    assert_eq!(agent.history.len(), 6);
    assert_eq!(
        function_call_name(&agent.history[1]).as_deref(),
        Some("run_shell")
    );
    assert_eq!(
        function_call_name(&agent.history[2]).as_deref(),
        Some("read_file")
    );
    let (first_id, first_output) =
        function_call_output(&agent.history[3]).expect("第三条应是工具结果");
    assert_eq!(first_id, "call-1");
    assert!(first_output.contains("run_shell"), "{first_output}");
    let (second_id, second_output) =
        function_call_output(&agent.history[4]).expect("第四条应是工具结果");
    assert_eq!(second_id, "call-2");
    assert!(second_output.contains("read_file"), "{second_output}");
}

#[tokio::test]
async fn committed_tool_step_survives_later_model_error() {
    let (mut agent, _state) = agent_with_replies(vec![
        StubReply::FunctionCall("run_shell"),
        StubReply::ModelError("服务端错误"),
    ]);
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    assert_eq!(ui.events, vec![UiEvent::Error("服务端错误".to_string())]);
    // 首步（用户输入 + 调用 + 结果）已完整提交；第二步失败只丢弃第二步输出。
    assert_eq!(agent.history.len(), 3);
}

#[tokio::test]
async fn set_model_clears_cached_llm_and_keeps_history() {
    let mut agent = agent_with(StubReply::Answer("第一次回答"));
    let mut ui = RecordingUi::default();
    agent.chat("你好", &mut ui).await;
    assert_eq!(agent.history.len(), 2);

    agent.set_model("nope/unknown-model");
    let mut ui = RecordingUi::default();
    agent.chat("再来", &mut ui).await;

    // 缓存已清空：不再走替身，而是按新规则构建并失败。
    assert_eq!(agent.history.len(), 2, "失败的一轮不应写入历史");
    match &ui.events[0] {
        UiEvent::Error(message) => assert!(message.contains("nope"), "{message}"),
        other => panic!("应展示错误，实际是 {other:?}"),
    }
}

#[tokio::test]
async fn with_model_overrides_default_model() {
    let config = Config {
        default_model: Some("other/model".to_string()),
        ..Config::default()
    };
    let mut agent = Agent::new(config).with_model("deepseek/deepseek-flash");
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    // 使用传入规则：按 deepseek 构建失败，而不是回落 default_model 的 other。
    match &ui.events[0] {
        UiEvent::Error(message) => {
            assert!(message.contains("deepseek"), "{message}");
        }
        other => panic!("应展示错误，实际是 {other:?}"),
    }
}

#[test]
fn new_falls_back_to_default_model() {
    let config = Config {
        default_model: Some("deepseek/deepseek-flash".to_string()),
        ..Config::default()
    };

    let agent = Agent::new(config);

    assert_eq!(agent.model.as_deref(), Some("deepseek/deepseek-flash"));
}

#[tokio::test]
async fn new_without_any_model_reports_missing_model() {
    let mut agent = Agent::new(Config::default());
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    match &ui.events[0] {
        UiEvent::Error(message) => assert!(message.contains("还没配置模型"), "{message}"),
        other => panic!("应展示错误，实际是 {other:?}"),
    }
    assert!(agent.history.is_empty());
}
