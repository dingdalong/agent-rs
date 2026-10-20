//! Agent：持有配置、当前模型规则与对话历史，负责推进一轮对话。
//!
//! Agent 不感知具体供应商：模型规则（`provider/model`）与 provider 连接信息
//! 都来自配置，实现由 [`crate::llm::build`] 选择。
//!
//! 历史按步骤提交：只有完整模型步骤写入历史，中断或失败的模型输出一律丢弃，
//! 用户输入随本轮首个完整步骤一起提交；工具调用与工具结果在工具执行完成后成对
//! 提交，并带着结果继续请求模型，直到模型不再调用工具。Agent 是历史的唯一写入者。
//!
//! 运行期失败在本模块内消化：提示用户后结束本轮，已提交的步骤保留。

use std::path::PathBuf;

use async_openai::types::responses::{EasyInputMessage, InputItem};

use crate::{
    config::Config,
    llm::{self, LLM},
    tools::{LlmToolPermissionJudge, ToolExecutor},
    ui::{Ui, UiEvent},
};

/// 对话 Agent：对话历史只在此处写入，保证状态来源单一。
pub struct Agent {
    /// 配置的 provider 表，切换模型时按需重建实现。
    config: Config,
    /// 工作目录：命令的默认工作目录与沙盒根；`with_tools` 重新装配工具时使用。
    workspace: PathBuf,
    /// 当前模型规则：创建时默认取配置的 `default_model`，可用 `with_model` 覆盖。
    model: Option<String>,
    /// 按当前模型规则惰性构建的实现。
    llm: Option<Box<dyn LLM>>,
    /// 工具执行器：按注册名选取好的内置工具，默认全部、可用 `with_tools` 收窄。
    tools: ToolExecutor,
    /// 对话历史（OpenAI 格式）；只在本模块内写入。
    history: Vec<InputItem>,
}

impl Agent {
    /// 用配置与工作目录创建 Agent：模型取 `default_model`，工具注册全部内置项。
    ///
    /// 工作目录既是命令的默认工作目录，也是沙盒默认可写的项目根。
    pub fn new(config: Config, workspace: PathBuf) -> Self {
        let model = config.default_model.clone();
        let tools = ToolExecutor::builtin(None, &config, &workspace);
        Self {
            config,
            workspace,
            model,
            llm: None,
            tools,
            history: Vec::new(),
        }
    }

    /// 指定模型规则，覆盖 `default_model`；供多个 Agent 各用不同模型。
    // 目前只有测试会用到，保留为多 Agent 场景的程序入口。
    #[allow(dead_code)]
    pub fn with_model(mut self, model: &str) -> Self {
        self.set_model(model);
        self
    }

    /// 切换模型规则：清空已构建的实现，历史保留。
    // 目前只有测试会用到，保留为会话中途切换模型的程序入口。
    #[allow(dead_code)]
    pub fn set_model(&mut self, model: &str) {
        if self.model.as_deref() == Some(model) {
            return;
        }
        self.model = Some(model.to_string());
        self.llm = None;
    }

    /// 指定本 Agent 使用的工具（按注册名）；不调用时注册全部内置工具。
    ///
    /// 名称不存在或重复时记录日志并跳过，不影响会话继续；空名单表示不注册任何工具。
    // 目前只有测试会用到，保留为多 Agent 场景的程序入口。
    #[allow(dead_code)]
    pub fn with_tools(mut self, names: &[&str]) -> Self {
        self.tools = ToolExecutor::builtin(Some(names), &self.config, &self.workspace);
        self
    }

    /// 推进一轮对话：按步骤请求模型，成功一步提交一步，失败只提示并结束本轮。
    ///
    /// 阶段约定：只有完整步骤写入历史，中断的模型输出一律丢弃；用户输入随
    /// 本轮首个完整步骤一起提交。模型请求工具调用时，在提交前执行工具，把调用
    /// 与结果成对写入历史，再带着结果继续请求模型；模型不再调用工具即结束本轮。
    pub async fn chat(&mut self, user_input: &str, ui: &mut (impl Ui + Send)) {
        if self.llm.is_none()
            && let Err(message) = self.ensure_llm()
        {
            tracing::error!(%message, "构建 LLM 失败");
            ui.render(UiEvent::Error(message));
            return;
        }

        // 待提交的用户输入：随首个完整步骤写入，失败时随本轮丢弃。
        let mut pending_input = Some(EasyInputMessage::from(user_input).into());

        loop {
            let request = self.request_items(pending_input.as_ref());
            let tools = self.tools.definitions();
            let llm = self.llm.as_ref().expect("LLM 已在上面构建成功");
            let outcome = match llm.chat(request, tools, ui).await {
                Ok(outcome) => outcome,
                // 原始错误已在供应商实现内记录；已提交的步骤保留，本轮就此结束。
                Err(message) => {
                    ui.render(UiEvent::Error(message));
                    return;
                }
            };

            // 工具结果与调用同一步提交，保证历史里不会出现没有结果的函数调用。
            let mut committed = outcome.output_items;
            let judge = LlmToolPermissionJudge::new(llm.as_ref());
            let tool_outputs = self
                .tools
                .execute_calls(&committed, user_input, &judge, ui)
                .await;
            let needs_follow_up = !tool_outputs.is_empty();
            committed.extend(tool_outputs);
            self.commit_step(pending_input.take(), committed);

            if !needs_follow_up {
                ui.render(UiEvent::TurnFinished);
                return;
            }
        }
    }

    /// 按当前模型规则构建实现；规则缺失或构建失败时返回面向用户的错误。
    fn ensure_llm(&mut self) -> Result<(), String> {
        let Some(model) = self.model.clone() else {
            return Err(
                "还没配置模型：请在 config.toml 的 [llm] 设置 default_model，例如 \"deepseek/deepseek-flash\""
                    .to_string(),
            );
        };
        self.llm = Some(llm::build(&self.config, &model)?);
        Ok(())
    }

    /// 组装本步骤要发送的请求 items：已提交历史 + 待提交输入（仅首步有）。
    fn request_items(&self, pending_input: Option<&InputItem>) -> Vec<InputItem> {
        let mut items = self.history.clone();
        if let Some(input) = pending_input {
            items.push(input.clone());
        }
        items
    }

    /// 提交一个完整步骤：首个步骤连同用户输入一起写入历史。
    ///
    /// 带工具调用的步骤在工具执行完成、结果齐全后整步提交，保证历史里不会
    /// 出现没有结果的函数调用。
    fn commit_step(&mut self, pending_input: Option<InputItem>, output_items: Vec<InputItem>) {
        if let Some(input) = pending_input {
            self.history.push(input);
        }
        self.history.extend(output_items);
    }
}

#[cfg(test)]
#[path = "../unit_tests/agent.rs"]
mod tests;
