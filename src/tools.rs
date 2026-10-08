//! 工具层：持有已注册的工具，把模型请求的函数调用执行成可回写历史的结果。
//!
//! 当前注册表为空：模型请求的调用一律按「工具不存在」生成结果。工具不存在与
//! 工具执行失败都属于工具结果，随调用一起写入历史并回给模型，不结束本轮对话。

use async_openai::types::responses::{
    FunctionCallOutput, FunctionCallOutputItemParam, InputItem, Item,
};

/// 一个可被模型调用的工具；`arguments` 的解析由实现自行负责。
pub trait Tool: Send + Sync {
    /// 工具名：模型按此名发起调用。
    fn name(&self) -> &str;

    /// 执行一次调用：`arguments` 是模型给出的 JSON 字符串。
    fn call(&self, arguments: &str) -> Result<String, String>;
}

/// 工具注册表与执行入口。
pub struct ToolExecutor {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolExecutor {
    /// 空注册表：模型请求的调用全部按「工具不存在」处理。
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// 注册给定工具。
    // 目前只有测试会用到，保留为后续真实工具注册的程序入口。
    #[allow(dead_code)]
    pub fn with_tools(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }

    /// 按序执行一个模型步骤里的全部函数调用，返回可写入历史的调用结果。
    pub fn execute_calls(&self, items: &[InputItem]) -> Vec<InputItem> {
        items
            .iter()
            .filter_map(|item| {
                if let InputItem::Item(Item::FunctionCall(call)) = item {
                    Some(self.execute(&call.name, &call.call_id, &call.arguments))
                } else {
                    None
                }
            })
            .collect()
    }

    /// 执行一次函数调用；工具不存在或执行失败都转成面向模型的结果文本。
    fn execute(&self, name: &str, call_id: &str, arguments: &str) -> InputItem {
        let text = if let Some(tool) = self.tools.iter().find(|tool| tool.name() == name) {
            match tool.call(arguments) {
                Ok(output) => output,
                Err(message) => {
                    tracing::error!(tool = name, %message, "工具执行失败");
                    message
                }
            }
        } else {
            tracing::error!(tool = name, "模型请求了不存在的工具");
            format!("工具「{name}」不存在。当前可用工具：{}", self.tool_names())
        };
        InputItem::Item(Item::FunctionCallOutput(FunctionCallOutputItemParam {
            call_id: call_id.to_string(),
            output: FunctionCallOutput::Text(text),
            id: None,
            status: None,
        }))
    }

    /// 可用工具名列表；一个都没有时返回「无」。
    fn tool_names(&self) -> String {
        if self.tools.is_empty() {
            return "无".to_string();
        }
        self.tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>()
            .join("、")
    }
}

#[cfg(test)]
#[path = "../unit_tests/tools.rs"]
mod tests;
