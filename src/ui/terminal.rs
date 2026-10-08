//! 终端实现：提示符、输入、事件分流均在此定义。
//!
//! IO 失败在本模块内记录日志并降级：读取失败结束会话，写入失败不影响会话。
//!
//! 输出约定：正文、思考与输入提示写 stdout，错误与会话结束写 stderr。
//! 思考块首写 `[思考] `、正文块首写 `[助手] `，前缀只在块首出现一次；
//! 块切换与回合收尾只补换行，不空行，保证每条输出都从行首开始。

use std::io::{self, BufRead, IsTerminal, Write};

use crate::ui::{Ui, UiEvent};

/// 会话结束指令，命中即返回 `None`。
const EXIT_COMMANDS: [&str; 3] = ["exit", "quit", "/exit"];
/// 思考块首前缀，与回答正文区分。
const REASONING_PREFIX: &str = "[思考] ";
/// 正文块首前缀，与思考过程区分。
const ANSWER_PREFIX: &str = "[助手] ";

/// 终端 UI：正文、思考与提示符写 stdout，错误与会话结束写 stderr。
pub struct TerminalUi {
    /// 输出格式状态，跨事件保持。
    formatter: OutputFormatter,
}

impl TerminalUi {
    /// 创建终端 UI。
    pub fn new() -> Self {
        Self {
            formatter: OutputFormatter::default(),
        }
    }
}

impl Default for TerminalUi {
    fn default() -> Self {
        Self::new()
    }
}

impl Ui for TerminalUi {
    fn read_input(&mut self) -> Option<String> {
        if let Err(error) = show_prompt() {
            tracing::error!(%error, "输出输入提示失败");
        }

        let mut line = String::new();
        let read = io::stdin().lock().read_line(&mut line);

        // 终端输入靠回显补换行；非终端输入、EOF 与读取失败都要由程序收尾，
        // 否则后续输出会接在提示符后面。
        let echo_handles_newline = match read {
            Ok(count) if count > 0 => io::stdin().is_terminal(),
            _ => false,
        };
        if !echo_handles_newline && let Err(error) = finish_prompt_line() {
            tracing::error!(%error, "收尾输入提示失败");
        }

        match read {
            Ok(0) => return None,
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, "读取用户输入失败");
                return None;
            }
        }

        let input = line.trim();
        if is_exit_command(input) {
            return None;
        }
        Some(input.to_string())
    }

    fn render(&mut self, event: UiEvent) {
        if let Err(error) = write_event(&mut self.formatter, &event) {
            tracing::error!(%error, "渲染输出事件失败");
        }
    }
}

/// 展示输入提示；失败只影响提示本身，不阻断读取。
fn show_prompt() -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "你：")?;
    stdout.flush()
}

/// 收尾提示符所在行：终端输入的换行由回显提供，其余情况由程序补一个换行。
fn finish_prompt_line() -> io::Result<()> {
    let mut stdout = io::stdout();
    writeln!(stdout)?;
    stdout.flush()
}

/// 输出目标流。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputStream {
    /// 正文、思考与提示符。
    Stdout,
    /// 错误与会话结束提示。
    Stderr,
}

/// 已格式化、待写入的输出片段。
#[derive(Debug, PartialEq, Eq)]
struct OutputChunk {
    /// 片段写往的流。
    stream: OutputStream,
    /// 片段文本。
    text: String,
}

/// 输出块类型，决定块首前缀是否已经写过。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputBlock {
    /// 思考块，块首写 `[思考] `。
    Reasoning,
    /// 正文块，块首写 `[助手] `。
    Answer,
}

/// 流式输出格式化器：把语义事件整理成带前缀与换行的输出片段。
///
/// 前缀只在块首写一次；块切换、回合收尾与错误提示都先闭合未结束的行，
/// 保证终端上每条输出都从行首开始。
#[derive(Default)]
struct OutputFormatter {
    /// 当前块；`None` 表示本轮尚无输出或已经收尾。
    block: Option<OutputBlock>,
    /// 当前未闭合行所在的流；`None` 表示位于行首。
    open_stream: Option<OutputStream>,
}

impl OutputFormatter {
    /// 把一条事件格式化成待写入的片段。
    fn format(&mut self, event: &UiEvent) -> Vec<OutputChunk> {
        let mut chunks = Vec::new();
        match event {
            UiEvent::AnswerText(text) => {
                if text.is_empty() {
                    return chunks;
                }
                self.start_block(OutputBlock::Answer, ANSWER_PREFIX, &mut chunks);
                self.write_text(text, &mut chunks);
            }
            UiEvent::ReasoningText(text) => {
                if text.is_empty() {
                    return chunks;
                }
                self.start_block(OutputBlock::Reasoning, REASONING_PREFIX, &mut chunks);
                self.write_text(text, &mut chunks);
            }
            UiEvent::TurnFinished => {
                self.close_line(&mut chunks);
                self.block = None;
            }
            UiEvent::Error(message) => {
                self.close_line(&mut chunks);
                self.block = None;
                chunks.push(OutputChunk {
                    stream: OutputStream::Stderr,
                    text: format!("错误：{message}\n"),
                });
            }
            UiEvent::Farewell => {
                self.close_line(&mut chunks);
                self.block = None;
                chunks.push(OutputChunk {
                    stream: OutputStream::Stderr,
                    text: "会话结束。\n".to_string(),
                });
            }
        }
        chunks
    }

    /// 开始新块：先闭合上一行并补一个换行，再写块首前缀；块内不重复写前缀。
    fn start_block(&mut self, block: OutputBlock, prefix: &str, chunks: &mut Vec<OutputChunk>) {
        if self.block == Some(block) {
            return;
        }
        self.close_line(chunks);
        self.block = Some(block);
        chunks.push(OutputChunk {
            stream: OutputStream::Stdout,
            text: prefix.to_string(),
        });
    }

    /// 追加一段增量文本，并按末字符更新行状态。
    fn write_text(&mut self, text: &str, chunks: &mut Vec<OutputChunk>) {
        chunks.push(OutputChunk {
            stream: OutputStream::Stdout,
            text: text.to_string(),
        });
        self.open_stream = if text.ends_with('\n') {
            None
        } else {
            Some(OutputStream::Stdout)
        };
    }

    /// 闭合未结束的行：补一个换行；已位于行首则不动。
    fn close_line(&mut self, chunks: &mut Vec<OutputChunk>) {
        if let Some(stream) = self.open_stream.take() {
            chunks.push(OutputChunk {
                stream,
                text: "\n".to_string(),
            });
        }
    }
}

/// 把一条事件渲染成输出片段并写到对应流。
fn write_event(formatter: &mut OutputFormatter, event: &UiEvent) -> io::Result<()> {
    for chunk in formatter.format(event) {
        match chunk.stream {
            OutputStream::Stdout => {
                let mut stdout = io::stdout();
                write!(stdout, "{}", chunk.text)?;
                stdout.flush()?;
            }
            OutputStream::Stderr => {
                let mut stderr = io::stderr();
                write!(stderr, "{}", chunk.text)?;
                stderr.flush()?;
            }
        }
    }
    Ok(())
}

/// 空行与结束指令都表示退出。
pub(crate) fn is_exit_command(input: &str) -> bool {
    input.is_empty() || EXIT_COMMANDS.contains(&input)
}

#[cfg(test)]
#[path = "../../unit_tests/ui/terminal.rs"]
mod tests;
