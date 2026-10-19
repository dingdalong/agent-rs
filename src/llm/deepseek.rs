//! `DeepSeek` 官方 Responses 接口实现。
//!
//! `DeepSeek` 端是无状态实现：不支持 `previous_response_id`、`conversation`、`store`，
//! 多轮上下文需要每轮把完整 items 原样回传（由 Agent 组装）。
//!
//! 失败在本模块内记录根因日志并转成面向用户的文本，不向上传递错误类型。

use std::{future::Future, pin::Pin};

use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::responses::{CreateResponseArgs, InputItem, ResponseStreamEvent, Tool as LlmTool},
};
use futures_util::{Stream, StreamExt};

use crate::{
    llm::{LLM, StepOutcome},
    ui::{Ui, UiEvent},
};

/// `DeepSeek` 返回的原始响应事件流。
type EventStream = Pin<Box<dyn Stream<Item = Result<ResponseStreamEvent, OpenAIError>> + Send>>;

/// 带模型名的 `DeepSeek` Responses 客户端。
pub(crate) struct DeepSeekClient {
    client: Client<OpenAIConfig>,
    model: String,
}

impl DeepSeekClient {
    /// 用 provider 的连接信息、凭据与模型名构造客户端。
    pub(crate) fn new(url: &str, api_key: &str, model: &str) -> Self {
        let openai_config = OpenAIConfig::new().with_api_base(url).with_api_key(api_key);
        Self {
            client: Client::with_config(openai_config),
            model: model.to_string(),
        }
    }
}

impl LLM for DeepSeekClient {
    fn chat<'a>(
        &'a self,
        input: Vec<InputItem>,
        tools: Vec<LlmTool>,
        ui: &'a mut (dyn Ui + Send),
    ) -> Pin<Box<dyn Future<Output = Result<StepOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            let stream = match respond(self, input, tools).await {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::error!(%error, model = %self.model, "发起对话请求失败");
                    return Err(error.to_string());
                }
            };
            collect_turn(stream, ui).await
        })
    }
}

/// 发起一次流式请求：items 与工具定义由调用方组装，这里原样发送。
async fn respond(
    client: &DeepSeekClient,
    input: Vec<InputItem>,
    tools: Vec<LlmTool>,
) -> Result<EventStream, OpenAIError> {
    tracing::debug!(model = %client.model, "发送模型请求");
    let mut builder = CreateResponseArgs::default();
    fill_request(&mut builder, &client.model, input, tools);
    let request = builder.build()?;
    let stream = client.client.responses().create_stream(request).await?;
    Ok(Box::pin(stream.filter(|event| {
        futures_util::future::ready(skip_stream_error(event))
    })))
}

/// 把模型规则、历史 items 与工具定义填进请求构造器；单独抽出来便于测试断言下发内容。
fn fill_request(
    builder: &mut CreateResponseArgs,
    model: &str,
    input: Vec<InputItem>,
    tools: Vec<LlmTool>,
) {
    builder.model(model.to_string()).input(input);
    // 没有工具时不下发空数组，避免供应商对空列表的处理差异。
    if !tools.is_empty() {
        builder.tools(tools);
    }
}

/// 消费一个模型步骤的流：推送语义事件，并在结束事件上返回结果。
async fn collect_turn<S>(mut stream: S, ui: &mut (dyn Ui + Send)) -> Result<StepOutcome, String>
where
    S: Stream<Item = Result<ResponseStreamEvent, OpenAIError>> + Unpin,
{
    while let Some(event) = stream.next().await {
        match event {
            Ok(ResponseStreamEvent::ResponseOutputTextDelta(delta)) => {
                ui.render(UiEvent::AnswerText(delta.delta));
            }
            Ok(ResponseStreamEvent::ResponseReasoningTextDelta(delta)) => {
                ui.render(UiEvent::ReasoningText(delta.delta));
            }
            Ok(ResponseStreamEvent::ResponseCompleted(completed)) => {
                let text = completed.response.output_text().unwrap_or_default();
                return Ok(StepOutcome {
                    output_items: completed
                        .response
                        .output
                        .into_iter()
                        .map(InputItem::from)
                        .collect(),
                    text,
                });
            }
            Ok(ResponseStreamEvent::ResponseIncomplete(incomplete)) => {
                tracing::error!(
                    details = ?incomplete.response.incomplete_details,
                    "响应被截断"
                );
                return Err(format!(
                    "响应被截断：{:?}",
                    incomplete.response.incomplete_details
                ));
            }
            Ok(ResponseStreamEvent::ResponseFailed(failed)) => {
                tracing::error!(error = ?failed.response.error, "响应失败");
                return Err(format!("响应失败：{:?}", failed.response.error));
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, "读取流式响应失败");
                return Err(error.to_string());
            }
        }
    }
    tracing::error!("流在结束事件前中断");
    Err("流在结束事件前中断".to_string())
}

/// 过滤 async-openai 类型定义外的事件：`DeepSeek` 会发送本地扩展事件，
/// 单个事件解析失败不应中断整轮对话；其他错误照常上报。
fn skip_stream_error(event: &Result<ResponseStreamEvent, OpenAIError>) -> bool {
    if let Err(error @ OpenAIError::JSONDeserialize(_, _)) = event {
        tracing::debug!(%error, "跳过无法解析的流事件");
        return false;
    }
    true
}

#[cfg(test)]
#[path = "../../unit_tests/llm/deepseek.rs"]
mod tests;
