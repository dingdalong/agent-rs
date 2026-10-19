//! UI 层：接管全部输入输出，Agent 只负责推送语义化事件。
//!
//! 输入输出失败由实现自行记录日志并降级：读取失败等同于结束会话，
//! 渲染失败不影响会话推进，因此这里不向上返回错误。

pub mod terminal;

/// 面向用户的输出事件，后续工具调用、工具结果、日志等类型在此扩展。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// 回答正文增量。
    AnswerText(String),
    /// 思考过程增量。
    ReasoningText(String),
    /// 模型发起一次工具调用；`summary` 是调用的一行摘要（如要执行的命令）。
    ToolCall { name: String, summary: String },
    /// 一次工具调用的结果文本。
    ToolResult(String),
    /// 本轮成功输出结束，接收方据此收尾换行并复位格式状态。
    TurnFinished,
    /// 需要展示给用户的错误。
    Error(String),
    /// 会话结束提示。
    Farewell,
}

/// 输入输出边界：实现方决定用户如何输入、事件如何呈现。
pub trait Ui {
    /// 展示提示符并读取一条输入；返回 `None` 表示结束会话
    /// （`exit`/`quit`/EOF，或读取失败被记录日志后按结束处理）。
    fn read_input(&mut self) -> Option<String>;

    /// 渲染一条输出事件；写入失败由实现记录日志，不影响会话继续。
    fn render(&mut self, event: UiEvent);
}
