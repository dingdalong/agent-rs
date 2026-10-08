use async_openai::{
    error::OpenAIError,
    types::responses::{EasyInputMessage, InputItem, ResponseStreamEvent},
};
use futures_util::stream;
use serde_json::json;

use super::{EventStream, collect_turn, skip_stream_error};
use crate::ui::{Ui, UiEvent};

/// 记录收到的事件，替代真实终端输出。
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

/// 把 JSON 数组解析成事件流，模拟 `DeepSeek` 的 SSE 输出。
fn stream_from(events: Vec<serde_json::Value>) -> EventStream {
    Box::pin(stream::iter(events.into_iter().map(|value| {
        serde_json::from_value::<ResponseStreamEvent>(value)
            .map_err(|error| OpenAIError::InvalidArgument(error.to_string()))
    })))
}

#[tokio::test]
async fn completed_turn_yields_text_and_items() {
    let events = stream_from(vec![
        json!({
            "type": "response.output_text.delta",
            "sequence_number": 1,
            "item_id": "item-1",
            "output_index": 0,
            "content_index": 0,
            "delta": "你好",
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "created_at": 1_700_000_000,
                "id": "resp-1",
                "model": "deepseek-flash",
                "object": "response",
                "status": "completed",
                "output": [
                    {
                        "id": "item-1",
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "你好，有什么可以帮你？",
                                "annotations": [],
                            }
                        ],
                    }
                ],
            },
        }),
    ]);
    let mut ui = RecordingUi::default();

    let outcome = collect_turn(events, &mut ui).await.expect("整轮应成功");

    assert_eq!(ui.events, vec![UiEvent::AnswerText("你好".to_string())]);
    assert_eq!(outcome.text, "你好，有什么可以帮你？");
    assert_eq!(outcome.output_items.len(), 1);
    assert!(matches!(outcome.output_items[0], InputItem::Item(_)));
}

#[tokio::test]
async fn reasoning_delta_is_forwarded() {
    let events = stream_from(vec![
        json!({
            "type": "response.reasoning_text.delta",
            "sequence_number": 1,
            "item_id": "reasoning-1",
            "output_index": 0,
            "content_index": 0,
            "delta": "先理解问题",
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "created_at": 1_700_000_000,
                "id": "resp-2",
                "model": "deepseek-flash",
                "object": "response",
                "status": "completed",
                "output": [],
            },
        }),
    ]);
    let mut ui = RecordingUi::default();

    let outcome = collect_turn(events, &mut ui).await.expect("整轮应成功");

    assert_eq!(
        ui.events,
        vec![UiEvent::ReasoningText("先理解问题".to_string())]
    );
    assert!(outcome.output_items.is_empty());
}

#[tokio::test]
async fn failed_response_returns_error() {
    let events = stream_from(vec![json!({
        "type": "response.failed",
        "sequence_number": 1,
        "response": {
            "created_at": 1_700_000_000,
            "id": "resp-3",
            "model": "deepseek-flash",
            "object": "response",
            "status": "failed",
            "error": {
                "code": "server_error",
                "message": "服务端错误",
            },
            "output": [],
        },
    })]);
    let mut ui = RecordingUi::default();

    let error = collect_turn(events, &mut ui).await.expect_err("应返回错误");

    assert!(error.contains("服务端错误"), "{error}");
}

#[tokio::test]
async fn incomplete_response_returns_error() {
    let events = stream_from(vec![json!({
        "type": "response.incomplete",
        "sequence_number": 1,
        "response": {
            "created_at": 1_700_000_000,
            "id": "resp-4",
            "model": "deepseek-flash",
            "object": "response",
            "status": "incomplete",
            "incomplete_details": {
                "reason": "max_output_tokens",
            },
            "output": [],
        },
    })]);
    let mut ui = RecordingUi::default();

    let error = collect_turn(events, &mut ui).await.expect_err("应返回错误");

    assert!(error.contains("响应被截断"), "{error}");
    assert!(error.contains("max_output_tokens"), "{error}");
}

#[tokio::test]
async fn stream_ended_before_completion_returns_error() {
    let events = stream_from(vec![json!({
        "type": "response.output_text.delta",
        "sequence_number": 1,
        "item_id": "item-1",
        "output_index": 0,
        "content_index": 0,
        "delta": "你好",
    })]);
    let mut ui = RecordingUi::default();

    let error = collect_turn(events, &mut ui).await.expect_err("应返回错误");

    assert!(error.contains("流在结束事件前中断"), "{error}");
}

#[test]
fn unknown_event_is_skipped() {
    let error = serde_json::from_value::<ResponseStreamEvent>(json!({
        "type": "response.deepseek_beta_event",
        "sequence_number": 1,
        "payload": "未知事件",
    }))
    .expect_err("未知事件类型应解析失败");
    let event = Err(OpenAIError::JSONDeserialize(
        error,
        json!({ "type": "response.deepseek_beta_event" }).to_string(),
    ));

    assert!(!skip_stream_error(&event));
}

#[test]
fn other_errors_are_propagated() {
    let event = Err(OpenAIError::InvalidArgument("请求参数错误".to_string()));

    assert!(skip_stream_error(&event));
}

/// 回传历史依赖该序列化格式，`DeepSeek` 按 message item 解析。
#[test]
fn easy_input_message_serializes_as_user_message_item() {
    let item = InputItem::from(EasyInputMessage::from("你好"));

    let value = serde_json::to_value(&item).expect("序列化输入 item");

    assert_eq!(
        value,
        json!({
            "type": "message",
            "role": "user",
            "content": "你好",
        })
    );
}
