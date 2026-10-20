//! 工具层：持有已注册的工具，把模型请求的函数调用执行成可回写历史的结果。
//!
//! 工具自己声明对模型可见的名称、说明与参数 Schema；工具不存在与工具执行失败都
//! 属于工具结果，随调用一起写入历史并回给模型，不结束本轮对话。执行过程同时以
//! UI 事件展示（调用摘要、权限结论，以及实际执行后的结果），方便用户审计模型
//! 做了什么以及在哪一层被阻止。
//!
//! 每个已注册调用在真正执行前，都会先由当前会话模型做一次语义权限判断。判断
//! 只能允许或拒绝当前调用，不能修改沙盒策略；权限判断与沙盒都必须通过才执行。
//!
//! 内置工具统一登记在 [`BUILTIN_TOOLS`] 注册表：工具在自己的模块里声明注册名与
//! 构造函数，再在注册表登记一行。调用方按名称选取工具，不传名称时注册全部，
//! 名称不存在或重复时记录日志并跳过，不影响会话继续。

mod bash;
mod permission;

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
pub(crate) use permission::{LlmToolPermissionJudge, ToolPermissionJudge, ToolPermissionRequest};

/// 工具执行失败的来源与类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecutionError {
    /// 沙盒不可用、启动失败或检测到沙盒拒绝。
    Sandbox {
        /// 面向模型和用户的完整失败说明。
        message: String,
        /// 可审计的沙盒失败依据。
        evidence: SandboxFailureEvidence,
    },
    /// 工具自身的参数、等待或其他执行错误。
    Other(String),
}

/// 沙盒失败的结构化依据，用于日志和调用方分流。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxFailureEvidence {
    /// 启动自检发现沙盒不可用。
    Unavailable,
    /// 沙盒命令无法完成包装或启动。
    SetupFailed,
    /// 命令输出命中了沙盒限制特征。
    DeniedOutput,
}

impl SandboxFailureEvidence {
    /// 稳定的日志字段值。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::SetupFailed => "setup_failed",
            Self::DeniedOutput => "denied_output",
        }
    }
}

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

    /// 权限判断用的单行摘要；默认复用终端展示摘要。
    ///
    /// 参数不合法时返回错误，权限判断与工具执行都会被跳过。
    fn permission_summary(&self, arguments: &str) -> Result<String, String> {
        Ok(self.call_summary(arguments))
    }

    /// 执行一次调用：`arguments` 是模型给出的 JSON 字符串。
    fn call<'a>(
        &'a self,
        arguments: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolExecutionError>> + Send + 'a>>;
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
        user_request: &str,
        judge: &dyn ToolPermissionJudge,
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
            let text = self
                .execute(&call.name, &call.arguments, user_request, judge, ui)
                .await;
            results.push(function_call_output(&call.call_id, text));
        }
        results
    }

    /// 执行一次函数调用；工具不存在、权限拒绝或执行失败都转成结果文本。
    async fn execute(
        &self,
        name: &str,
        arguments: &str,
        user_request: &str,
        judge: &dyn ToolPermissionJudge,
        ui: &mut (dyn Ui + Send),
    ) -> String {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) else {
            tracing::error!(tool = name, "模型请求了不存在的工具");
            let text = format!("工具「{name}」不存在。当前可用工具：{}", self.tool_names());
            render_tool_result(ui, &text);
            return text;
        };
        let summary = match tool.permission_summary(arguments) {
            Ok(summary) => summary,
            Err(message) => {
                tracing::error!(tool = name, %message, "工具参数未通过权限预检");
                render_tool_result(ui, &message);
                return message;
            }
        };
        let request = ToolPermissionRequest::new(
            user_request,
            tool.name(),
            tool.description(),
            summary,
            arguments,
        );
        match judge.judge(request).await {
            Ok(decision) if decision.allowed => {
                tracing::debug!(
                    stage = "permission",
                    source = "llm_permission_judge",
                    decision = "allow",
                    evidence = "llm_result",
                    tool = name,
                    reason = %decision.reason,
                    "工具权限判断通过"
                );
                ui.render(UiEvent::ToolPermission {
                    allowed: true,
                    reason: decision.reason,
                });
            }
            Ok(decision) => {
                tracing::debug!(
                    stage = "permission",
                    source = "llm_permission_judge",
                    decision = "deny",
                    evidence = "llm_result",
                    tool = name,
                    reason = %decision.reason,
                    "工具权限判断拒绝"
                );
                ui.render(UiEvent::ToolPermission {
                    allowed: false,
                    reason: decision.reason.clone(),
                });
                return format!(
                    "工具「{name}」未执行：权限判断拒绝：{} 请调整方案或向用户说明。",
                    decision.reason
                );
            }
            Err(message) => {
                tracing::error!(
                    stage = "permission",
                    source = "llm_permission_judge",
                    decision = "error",
                    evidence = "judge_error",
                    tool = name,
                    %message,
                    "工具权限判断失败，按失败即拒绝处理"
                );
                let reason = format!("智能权限判断失败：{message}");
                ui.render(UiEvent::ToolPermission {
                    allowed: false,
                    reason: reason.clone(),
                });
                return format!(
                    "工具「{name}」未执行：{reason}。按失败即拒绝策略，本次调用已取消。"
                );
            }
        }
        match tool.call(arguments.to_string()).await {
            Ok(output) => {
                render_tool_result(ui, &output);
                output
            }
            Err(error) => {
                let text = tool_execution_failure_text(name, error);
                render_tool_result(ui, &text);
                text
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

/// 展示一次实际执行后的工具结果；权限拒绝只展示权限事件。
fn render_tool_result(ui: &mut (dyn Ui + Send), text: &str) {
    ui.render(UiEvent::ToolResult(text.to_string()));
}

/// 把结构化工具执行错误转成模型可读文本，并记录失败来源。
fn tool_execution_failure_text(name: &str, error: ToolExecutionError) -> String {
    match error {
        ToolExecutionError::Sandbox { message, evidence } => {
            tracing::error!(
                stage = "tool_execution",
                source = "sandbox",
                decision = "failure",
                evidence = evidence.as_str(),
                tool = name,
                %message,
                "工具执行被沙盒阻止"
            );
            format!("工具「{name}」执行失败：沙盒拒绝：{message}")
        }
        ToolExecutionError::Other(message) => {
            tracing::error!(
                stage = "tool_execution",
                source = "tool",
                decision = "failure",
                tool = name,
                %message,
                "工具执行失败"
            );
            message
        }
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
