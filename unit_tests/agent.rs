use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_openai::types::responses::{InputItem, OutputItem, Tool as LlmTool};
use serde_json::json;

use super::Agent;
use crate::{
    config::Config,
    llm::{LLM, StepOutcome},
    tools::{Tool, ToolExecutionError, ToolExecutor, ToolPermissionRequest},
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
    judgement: StubJudgement,
}

/// 替身共享状态，供用例在请求发出后断言。
#[derive(Default)]
struct StubState {
    requests: Mutex<Vec<Vec<InputItem>>>,
    tools: Mutex<Vec<Vec<LlmTool>>>,
    judge_requests: Mutex<Vec<ToolPermissionRequest>>,
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

    /// 最近一次请求下发给模型的工具定义。
    fn last_tools(&self) -> Vec<LlmTool> {
        self.tools
            .lock()
            .expect("工具记录锁未被污染")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// 权限判断收到的全部请求。
    fn judge_requests(&self) -> Vec<ToolPermissionRequest> {
        self.judge_requests
            .lock()
            .expect("权限请求记录锁未被污染")
            .clone()
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

/// 权限判断替身的预设结论。
#[derive(Clone, Copy)]
enum StubJudgement {
    /// 允许执行。
    Allow,
    /// 拒绝执行。
    Deny(&'static str),
    /// 判断本身失败。
    Error(&'static str),
}

impl LLM for StubLlm {
    fn chat(
        &self,
        input: Vec<InputItem>,
        tools: Vec<LlmTool>,
        ui: &mut (dyn Ui + Send),
    ) -> Pin<Box<dyn Future<Output = Result<StepOutcome, String>> + Send + '_>> {
        if let Some(request) = permission_request(&input) {
            self.state
                .judge_requests
                .lock()
                .expect("权限请求记录锁未被污染")
                .push(request);
            ui.render(UiEvent::AnswerText("权限判断内部输出".to_string()));
            let judgement = self.judgement;
            return Box::pin(async move {
                match judgement {
                    StubJudgement::Allow => Ok(StepOutcome {
                        output_items: Vec::new(),
                        text: r#"{"allowed":true,"reason":"与当前请求相关"}"#.to_string(),
                    }),
                    StubJudgement::Deny(reason) => Ok(StepOutcome {
                        output_items: Vec::new(),
                        text: format!(r#"{{"allowed":false,"reason":"{reason}"}}"#),
                    }),
                    StubJudgement::Error(message) => Err(message.to_string()),
                }
            });
        }
        self.state
            .requests
            .lock()
            .expect("请求记录锁未被污染")
            .push(input);
        self.state
            .tools
            .lock()
            .expect("工具记录锁未被污染")
            .push(tools);
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

/// 从权限适配器发出的 system/user 消息还原审查请求。
fn permission_request(input: &[InputItem]) -> Option<ToolPermissionRequest> {
    if input.len() != 2 {
        return None;
    }
    let system = serde_json::to_value(&input[0]).ok()?;
    let user = serde_json::to_value(&input[1]).ok()?;
    if system["role"] != "system" || user["role"] != "user" {
        return None;
    }
    let payload: serde_json::Value = serde_json::from_str(user["content"].as_str()?).ok()?;
    let tool = payload.get("tool")?;
    Some(ToolPermissionRequest::new(
        payload["current_user_request"].as_str()?,
        tool["name"].as_str()?,
        tool["description"].as_str()?,
        tool["summary"].as_str()?.to_string(),
        tool["arguments"].as_str()?,
    ))
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

/// 从事件序列里取出工具调用摘要与结果文本，忽略其它事件。
fn tool_events(events: &[UiEvent]) -> (Vec<(String, String)>, Vec<String>) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for event in events {
        match event {
            UiEvent::ToolCall { name, summary } => calls.push((name.clone(), summary.clone())),
            UiEvent::ToolResult(text) => results.push(text.clone()),
            _ => {}
        }
    }
    (calls, results)
}

/// 注入替身的 Agent，不发起真实请求。
fn agent_with(reply: StubReply) -> Agent {
    agent_with_replies(vec![reply]).0
}

/// 注入多步替身回复的 Agent，并返回共享状态供请求断言。
fn agent_with_replies(replies: Vec<StubReply>) -> (Agent, Arc<StubState>) {
    let state = Arc::new(StubState::default());
    let mut agent = Agent::new(Config::default(), workspace()).with_model("test/model");
    agent.llm = Some(stub_llm(replies, Arc::clone(&state)));
    (agent, state)
}

/// 用例用的工作目录：真实存在的临时目录，供沙盒策略使用。
fn workspace() -> std::path::PathBuf {
    std::env::temp_dir()
}

/// 构造按序返回给定回复的替身。
fn stub_llm(replies: Vec<StubReply>, state: Arc<StubState>) -> Box<dyn LLM> {
    stub_llm_with_judgement(replies, state, StubJudgement::Allow)
}

/// 构造带指定权限判断结论的替身。
fn stub_llm_with_judgement(
    replies: Vec<StubReply>,
    state: Arc<StubState>,
    judgement: StubJudgement,
) -> Box<dyn LLM> {
    Box::new(StubLlm {
        replies: Mutex::new(replies.into()),
        state,
        judgement,
    })
}

/// 注入权限判断结论与多步回复的 Agent，并返回共享状态。
fn agent_with_judgement(
    replies: Vec<StubReply>,
    judgement: StubJudgement,
) -> (Agent, Arc<StubState>) {
    let state = Arc::new(StubState::default());
    let mut agent = Agent::new(Config::default(), workspace()).with_model("test/model");
    agent.llm = Some(stub_llm_with_judgement(
        replies,
        Arc::clone(&state),
        judgement,
    ));
    (agent, state)
}

/// 回显工具，用于确认权限拒绝时工具没有被执行。
struct EchoTool {
    executions: Arc<AtomicUsize>,
}

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "回显调用参数"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn call<'a>(
        &'a self,
        arguments: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolExecutionError>> + Send + 'a>> {
        self.executions.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { Ok(format!("回显：{arguments}")) })
    }
}

/// 把 Agent 的工具替换成可计数的 echo，便于观察权限门控是否真正阻止执行。
fn with_echo_tool(mut agent: Agent) -> (Agent, Arc<AtomicUsize>) {
    let executions = Arc::new(AtomicUsize::new(0));
    agent.tools = ToolExecutor::with_tools(vec![Box::new(EchoTool {
        executions: Arc::clone(&executions),
    })]);
    (agent, executions)
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
    let (calls, results) = tool_events(&ui.events);
    assert_eq!(calls, vec![("run_shell".to_string(), "{}".to_string())]);
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("不存在"), "{}", results[0]);
    assert_eq!(ui.events.last(), Some(&UiEvent::TurnFinished));
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

    let (calls, results) = tool_events(&ui.events);
    assert_eq!(
        calls,
        vec![
            ("run_shell".to_string(), "{}".to_string()),
            ("read_file".to_string(), "{}".to_string()),
        ]
    );
    assert_eq!(results.len(), 2);
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

    // 工具调用与结果先展示，随后第二步失败只报错。
    let (calls, results) = tool_events(&ui.events);
    assert_eq!(calls.len(), 1);
    assert_eq!(results.len(), 1);
    assert_eq!(
        ui.events.last(),
        Some(&UiEvent::Error("服务端错误".to_string()))
    );
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
    let mut agent = Agent::new(config, workspace()).with_model("deepseek/deepseek-flash");
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

    let agent = Agent::new(config, workspace());

    assert_eq!(agent.model.as_deref(), Some("deepseek/deepseek-flash"));
}

#[tokio::test]
async fn new_without_any_model_reports_missing_model() {
    let mut agent = Agent::new(Config::default(), workspace());
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    match &ui.events[0] {
        UiEvent::Error(message) => assert!(message.contains("还没配置模型"), "{message}"),
        other => panic!("应展示错误，实际是 {other:?}"),
    }
    assert!(agent.history.is_empty());
}

#[tokio::test]
async fn chat_sends_tool_definitions_to_the_model() {
    let (mut agent, state) = agent_with_replies(vec![StubReply::Answer("你好")]);
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    // 默认注册表里有 bash：请求必须带上它的名称与参数说明。
    let tools = state.last_tools();
    assert_eq!(tools.len(), 1);
    let value = serde_json::to_value(&tools[0]).expect("工具定义应可序列化");
    assert_eq!(value["type"], "function");
    assert_eq!(value["name"], "bash");
    assert_eq!(value["parameters"]["required"], json!(["command"]));
}

#[tokio::test]
async fn bash_call_result_is_committed_and_turn_continues() {
    let (mut agent, _state) = agent_with_replies(vec![
        StubReply::FunctionCall("bash"),
        StubReply::Answer("命令没跑成，我换个办法。"),
    ]);
    let mut ui = RecordingUi::default();

    agent.chat("看一下当前目录", &mut ui).await;

    // 替身给出的参数是空对象，属于参数错误：结果照样回写历史并继续本轮。
    let (calls, results) = tool_events(&ui.events);
    assert_eq!(calls, vec![("bash".to_string(), "{}".to_string())]);
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("参数不合法"), "{}", results[0]);
    assert_eq!(ui.events.last(), Some(&UiEvent::TurnFinished));
    // 历史：用户输入 + 函数调用 + 工具结果 + 最终回答。
    assert_eq!(agent.history.len(), 4);
    assert_eq!(
        function_call_name(&agent.history[1]).as_deref(),
        Some("bash")
    );
    assert_eq!(
        function_call_output(&agent.history[2])
            .expect("第三步应是工具结果")
            .1,
        results[0]
    );
}

#[tokio::test]
async fn tool_permission_receives_current_request_and_allows_execution() {
    let (agent, state) = agent_with_replies(vec![
        StubReply::FunctionCall("echo"),
        StubReply::Answer("已经处理。"),
    ]);
    let (mut agent, executions) = with_echo_tool(agent);
    let mut ui = RecordingUi::default();

    agent.chat("请回显参数", &mut ui).await;

    assert_eq!(executions.load(Ordering::Relaxed), 1);
    let requests = state.judge_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].user_request, "请回显参数");
    assert_eq!(requests[0].tool_name, "echo");
    assert!(ui.events.contains(&UiEvent::ToolPermission {
        allowed: true,
        reason: "与当前请求相关".to_string(),
    }));
    assert!(
        !ui.events
            .contains(&UiEvent::AnswerText("权限判断内部输出".to_string()))
    );
    assert_eq!(ui.events.last(), Some(&UiEvent::TurnFinished));
    assert_eq!(agent.history.len(), 4);
}

#[tokio::test]
async fn denied_tool_never_executes_and_turn_continues() {
    let (agent, state) = agent_with_judgement(
        vec![
            StubReply::FunctionCall("echo"),
            StubReply::Answer("我改用其他方式。"),
        ],
        StubJudgement::Deny("调用与当前请求无关"),
    );
    let (mut agent, executions) = with_echo_tool(agent);
    let mut ui = RecordingUi::default();

    agent.chat("请不要执行无关操作", &mut ui).await;

    assert_eq!(executions.load(Ordering::Relaxed), 0);
    assert_eq!(state.judge_requests().len(), 1);
    assert!(ui.events.contains(&UiEvent::ToolPermission {
        allowed: false,
        reason: "调用与当前请求无关".to_string(),
    }));
    assert!(
        !ui.events
            .iter()
            .any(|event| matches!(event, UiEvent::ToolResult(_)))
    );
    let history_output = function_call_output(&agent.history[2])
        .expect("历史中仍应有工具结果")
        .1;
    assert!(history_output.contains("权限判断拒绝"), "{history_output}");
    assert_eq!(ui.events.last(), Some(&UiEvent::TurnFinished));
    assert_eq!(agent.history.len(), 4);
}

#[tokio::test]
async fn failed_permission_judgment_skips_execution_and_continues() {
    let (agent, state) = agent_with_judgement(
        vec![
            StubReply::FunctionCall("echo"),
            StubReply::Answer("权限服务异常，我暂时不执行。"),
        ],
        StubJudgement::Error("审查请求超时"),
    );
    let (mut agent, executions) = with_echo_tool(agent);
    let mut ui = RecordingUi::default();

    agent.chat("回显参数", &mut ui).await;

    assert_eq!(executions.load(Ordering::Relaxed), 0);
    assert_eq!(state.judge_requests().len(), 1);
    assert!(
        !ui.events
            .iter()
            .any(|event| matches!(event, UiEvent::ToolResult(_)))
    );
    let history_output = function_call_output(&agent.history[2])
        .expect("历史中仍应有工具结果")
        .1;
    assert!(history_output.contains("审查请求超时"), "{history_output}");
    assert!(history_output.contains("失败即拒绝"), "{history_output}");
    assert_eq!(ui.events.last(), Some(&UiEvent::TurnFinished));
    assert_eq!(agent.history.len(), 4);
}

#[tokio::test]
async fn permission_judgment_follows_the_rebuilt_llm() {
    let (agent, first_state) = agent_with_replies(vec![
        StubReply::FunctionCall("echo"),
        StubReply::Answer("第一次完成。"),
    ]);
    let (mut agent, executions) = with_echo_tool(agent);
    let mut ui = RecordingUi::default();
    agent.chat("第一次回显", &mut ui).await;
    assert_eq!(executions.load(Ordering::Relaxed), 1);
    assert_eq!(first_state.judge_requests().len(), 1);

    agent.set_model("test/other-model");
    let second_state = Arc::new(StubState::default());
    agent.llm = Some(stub_llm_with_judgement(
        vec![
            StubReply::FunctionCall("echo"),
            StubReply::Answer("第二次被拒绝。"),
        ],
        Arc::clone(&second_state),
        StubJudgement::Deny("新模型拒绝该调用"),
    ));
    let mut ui = RecordingUi::default();
    agent.chat("第二次回显", &mut ui).await;

    assert_eq!(executions.load(Ordering::Relaxed), 1);
    let requests = second_state.judge_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].user_request, "第二次回显");
    assert!(ui.events.contains(&UiEvent::ToolPermission {
        allowed: false,
        reason: "新模型拒绝该调用".to_string(),
    }));
}

#[tokio::test]
async fn with_tools_registers_only_selected_tools() {
    let (agent, state) = agent_with_replies(vec![StubReply::Answer("你好")]);
    let mut agent = agent.with_tools(&["bash"]);
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    let tools = state.last_tools();
    assert_eq!(tools.len(), 1);
    let value = serde_json::to_value(&tools[0]).expect("工具定义应可序列化");
    assert_eq!(value["name"], "bash");
    assert_eq!(ui.events, vec![UiEvent::TurnFinished]);
}

#[tokio::test]
async fn with_tools_skips_unknown_names_and_keeps_the_turn_running() {
    let (agent, state) = agent_with_replies(vec![StubReply::Answer("你好")]);
    let mut agent = agent.with_tools(&["run_shell"]);
    let mut ui = RecordingUi::default();

    agent.chat("你好", &mut ui).await;

    // 名称不存在只记录日志并跳过：请求不带任何工具，本轮照常完成。
    assert!(state.last_tools().is_empty());
    assert_eq!(ui.events, vec![UiEvent::TurnFinished]);
    assert_eq!(agent.history.len(), 2);
}
