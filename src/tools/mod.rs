//! 工具层：持有已注册的工具，把模型请求的函数调用执行成可回写历史的结果。
//!
//! 工具自己声明对模型可见的名称、说明与参数 Schema；工具不存在与工具执行失败都
//! 属于工具结果，随调用一起写入历史并回给模型，不结束本轮对话。执行过程同时以
//! UI 事件展示（调用摘要 + 结果），方便用户审计模型做了什么。
//!
//! 内置工具统一登记在 [`BUILTIN_TOOLS`] 注册表：工具在自己的模块里声明注册名与
//! 构造函数，再在注册表登记一行。调用方按名称选取工具，不传名称时注册全部，
//! 名称不存在或重复时记录日志并跳过，不影响会话继续。

mod bash;

use std::{future::Future, path::Path, pin::Pin};

use async_openai::types::responses::{
    FunctionCallOutput, FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, InputItem,
    Item, Tool as LlmTool,
};

use crate::{
    config::Config,
    ui::{Ui, UiEvent},
};

pub use bash::Bash;

/// 工具构造函数：按配置与工作目录装配一个工具。
type ToolBuild = fn(&Config, &Path) -> Box<dyn Tool>;

/// 项目内置工具注册表：注册名 → 构造函数。新增工具在末尾登记一行。
const BUILTIN_TOOLS: &[(&str, ToolBuild)] = &[(Bash::NAME, Bash::build)];

/// 一个可被模型调用的工具；`arguments` 的解析由实现自行负责。
pub trait Tool: Send + Sync {
    /// 工具名：模型按此名发起调用。
    fn name(&self) -> &str;

    /// 工具说明：模型据此判断什么时候调用。
    fn description(&self) -> &str;

    /// 参数的 JSON Schema。
    fn parameters(&self) -> serde_json::Value;

    /// 终端展示用的一行调用摘要；默认原样展示参数。
    fn call_summary(&self, arguments: &str) -> String {
        arguments.to_string()
    }

    /// 执行一次调用：`arguments` 是模型给出的 JSON 字符串。
    fn call<'a>(
        &'a self,
        arguments: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

/// 工具注册表与执行入口。
///
/// 内置工具用 [`ToolExecutor::builtin`] 按名称注册；测试替身等自定义工具用
/// [`ToolExecutor::with_tools`] 显式注册。
pub struct ToolExecutor {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolExecutor {
    /// 注册给定工具。
    pub fn with_tools(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }

    /// 注册项目内置工具：`names` 为 `None` 时注册全部。
    ///
    /// 名称不存在或重复时记录日志并跳过，不影响会话继续；空名单表示不注册任何工具。
    pub fn builtin(names: Option<&[&str]>, config: &Config, workspace: &Path) -> Self {
        let tools: Vec<Box<dyn Tool>> = match names {
            None => BUILTIN_TOOLS
                .iter()
                .map(|(_, build)| build(config, workspace))
                .collect(),
            Some(names) => select_tools(names, BUILTIN_TOOLS, config, workspace),
        };
        let executor = Self::with_tools(tools);
        tracing::info!(tools = %executor.tool_names(), "内置工具注册完成");
        executor
    }

    /// 转成发给模型的工具定义（`OpenAI` Responses 格式）。
    pub fn definitions(&self) -> Vec<LlmTool> {
        self.tools
            .iter()
            .map(|tool| {
                LlmTool::Function(FunctionTool {
                    name: tool.name().to_string(),
                    description: Some(tool.description().to_string()),
                    parameters: Some(tool.parameters()),
                    strict: None,
                    defer_loading: None,
                })
            })
            .collect()
    }

    /// 按序执行一个模型步骤里的全部函数调用，返回可写入历史的调用结果。
    pub async fn execute_calls(
        &self,
        items: &[InputItem],
        ui: &mut (dyn Ui + Send),
    ) -> Vec<InputItem> {
        let mut results = Vec::new();
        for item in items {
            let InputItem::Item(Item::FunctionCall(call)) = item else {
                continue;
            };
            ui.render(UiEvent::ToolCall {
                name: call.name.clone(),
                summary: self.call_summary(call),
            });
            tracing::debug!(tool = %call.name, "执行工具调用");
            let text = self.execute(&call.name, &call.arguments).await;
            ui.render(UiEvent::ToolResult(text.clone()));
            results.push(function_call_output(&call.call_id, text));
        }
        results
    }

    /// 执行一次函数调用；工具不存在或执行失败都转成面向模型的结果文本。
    async fn execute(&self, name: &str, arguments: &str) -> String {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) else {
            tracing::error!(tool = name, "模型请求了不存在的工具");
            return format!("工具「{name}」不存在。当前可用工具：{}", self.tool_names());
        };
        match tool.call(arguments.to_string()).await {
            Ok(output) => output,
            Err(message) => {
                tracing::error!(tool = name, %message, "工具执行失败");
                message
            }
        }
    }

    /// 终端展示用的一行摘要；工具不存在时退回展示原始参数。
    fn call_summary(&self, call: &FunctionToolCall) -> String {
        self.tools
            .iter()
            .find(|tool| tool.name() == call.name)
            .map_or_else(
                || call.arguments.clone(),
                |tool| tool.call_summary(&call.arguments),
            )
    }

    /// 可用工具名列表。
    fn tool_names(&self) -> String {
        self.tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>()
            .join("、")
    }
}

/// 按名称从注册表选取并装配工具：保持调用方给出的顺序，名称不存在或重复时记录
/// 日志并跳过。
fn select_tools(
    names: &[&str],
    registry: &[(&str, ToolBuild)],
    config: &Config,
    workspace: &Path,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    for name in names {
        let Some((_, build)) = registry.iter().find(|(tool_name, _)| tool_name == name) else {
            tracing::error!(
                tool = %name,
                available = %registry_names(registry),
                "选取的内置工具不存在，已跳过"
            );
            continue;
        };
        if tools.iter().any(|tool| tool.name() == *name) {
            tracing::error!(tool = %name, "选取的内置工具重复，已跳过重复项");
            continue;
        }
        tools.push(build(config, workspace));
    }
    tools
}

/// 注册表里的全部工具名，用于日志与错误提示。
fn registry_names(registry: &[(&str, ToolBuild)]) -> String {
    registry
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join("、")
}

/// 组装一条函数调用结果 item。
fn function_call_output(call_id: &str, text: String) -> InputItem {
    InputItem::Item(Item::FunctionCallOutput(FunctionCallOutputItemParam {
        call_id: call_id.to_string(),
        output: FunctionCallOutput::Text(text),
        id: None,
        status: None,
    }))
}

#[cfg(test)]
#[path = "../../unit_tests/tools.rs"]
mod tests;
