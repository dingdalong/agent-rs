//! 模型接入层：定义供应商无关的 LLM 接口，并按模型规则选择实现。
//!
//! 调用方只认 [`LLM`] 与 `provider/model` 规则，不需要知道具体供应商；
//! 供应商实现负责把 `OpenAI` 格式的 items 转换成自家请求格式。
//!
//! 失败只向上返回面向用户的中文说明，原始错误由实现记录日志，
//! 调用方展示后结束本轮即可，不需要按错误种类分支。

mod deepseek;

use std::{future::Future, pin::Pin};

use async_openai::types::responses::InputItem;

use crate::{config::Config, ui::Ui};

/// 一个模型步骤的产出：助手侧输出 item 用 `OpenAI` 格式，保证历史供应商无关。
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    /// 本步模型输出的全部 item。
    pub output_items: Vec<InputItem>,
    /// 本步模型输出的文本。
    pub text: String,
}

/// 供应商无关的对话接口。
// 按仓库约定，LLM 需要大写时整体大写，不采纳 clippy 建议的 `Llm`。
#[allow(clippy::upper_case_acronyms)]
pub trait LLM: Send + Sync {
    /// 发起一次模型请求（一个步骤）：`input` 是本步要发送的完整 items。
    ///
    /// 实现方负责把 items 转换成自家请求格式、发起请求、把流式增量渲染到 `ui`，
    /// 并返回 `OpenAI` 格式的本步输出 item；失败时返回面向用户的说明，
    /// 原始错误由实现记录日志。实现不保存任何对话状态。
    fn chat<'a>(
        &'a self,
        input: Vec<InputItem>,
        ui: &'a mut (dyn Ui + Send),
    ) -> Pin<Box<dyn Future<Output = Result<StepOutcome, String>> + Send + 'a>>;
}

/// 按 `provider/model` 规则构建实现。
pub fn build(config: &Config, model: &str) -> Result<Box<dyn LLM>, String> {
    let (provider_name, model_name) = split_model(model)?;
    let provider = config.provider(provider_name).ok_or_else(|| {
        format!(
            "配置里没有 provider「{provider_name}」：请在 config.toml 增加 [llm.providers.{provider_name}]"
        )
    })?;
    let url = provider.url.as_deref().ok_or_else(|| {
        format!(
            "provider「{provider_name}」缺少 url：请在 config.toml 的 [llm.providers.{provider_name}] 配置 url"
        )
    })?;
    let api_key = provider.api_key.as_deref().ok_or_else(|| {
        format!(
            "provider「{provider_name}」缺少 api_key：请在 auth.toml 的 [llm.providers.{provider_name}] 配置 api_key"
        )
    })?;

    match provider_name {
        "deepseek" => {
            let client = deepseek::DeepSeekClient::new(url, api_key, model_name);
            tracing::info!(
                provider = provider_name,
                model = model_name,
                "构建 LLM 实现"
            );
            Ok(Box::new(client))
        }
        other => Err(format!(
            "provider「{other}」还没有对应实现：请检查模型规则或等待后续支持"
        )),
    }
}

/// 解析 `provider/model`：按第一个 `/` 切分，两段都不能为空。
fn split_model(model: &str) -> Result<(&str, &str), String> {
    let Some((provider, name)) = model.split_once('/') else {
        return Err(format!(
            "模型规则「{model}」不合法：应为 provider/model，例如 deepseek/deepseek-flash"
        ));
    };
    let provider = provider.trim();
    let name = name.trim();
    if provider.is_empty() || name.is_empty() {
        return Err(format!(
            "模型规则「{model}」不合法：provider 与 model 都不能为空"
        ));
    }
    Ok((provider, name))
}

#[cfg(test)]
#[path = "../../unit_tests/llm.rs"]
mod tests;
