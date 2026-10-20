use super::{OutputChunk, OutputFormatter, OutputStream, is_exit_command};
use crate::ui::UiEvent;

/// 构造写往 stdout 的输出片段。
fn stdout(text: &str) -> OutputChunk {
    OutputChunk {
        stream: OutputStream::Stdout,
        text: text.to_string(),
    }
}

/// 构造写往 stderr 的输出片段。
fn stderr(text: &str) -> OutputChunk {
    OutputChunk {
        stream: OutputStream::Stderr,
        text: text.to_string(),
    }
}

/// 依次格式化多条事件，拼接输出片段。
fn format_all(events: &[UiEvent]) -> Vec<OutputChunk> {
    let mut formatter = OutputFormatter::default();
    let mut chunks = Vec::new();
    for event in events {
        chunks.extend(formatter.format(event));
    }
    chunks
}

#[test]
fn exit_commands_are_recognized() {
    assert!(is_exit_command("exit"));
    assert!(is_exit_command("quit"));
    assert!(is_exit_command("/exit"));
    assert!(is_exit_command(""));
}

#[test]
fn normal_input_is_not_exit() {
    assert!(!is_exit_command("你好"));
    assert!(!is_exit_command("exiting"));
}

#[test]
fn each_tool_call_starts_its_own_line() {
    let chunks = format_all(&[
        UiEvent::ToolCall {
            name: "bash".to_string(),
            summary: "ls".to_string(),
        },
        UiEvent::ToolCall {
            name: "bash".to_string(),
            summary: "pwd".to_string(),
        },
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[工具] "),
            stdout("bash：ls\n"),
            stdout("[工具] "),
            stdout("bash：pwd\n"),
        ]
    );
}

#[test]
fn permission_decision_gets_its_own_line_between_call_and_result() {
    let chunks = format_all(&[
        UiEvent::ToolCall {
            name: "bash".to_string(),
            summary: "rm obsolete.txt".to_string(),
        },
        UiEvent::ToolPermission {
            allowed: true,
            reason: "用户明确要求删除该文件".to_string(),
        },
        UiEvent::ToolResult("退出码：0\n".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[工具] "),
            stdout("bash：rm obsolete.txt\n"),
            stdout("[权限] "),
            stdout("允许：用户明确要求删除该文件\n"),
            stdout("[工具结果] "),
            stdout("退出码：0\n"),
        ]
    );
}

#[test]
fn denied_permission_reports_the_tool_was_not_executed() {
    let chunks = format_all(&[
        UiEvent::ToolCall {
            name: "bash".to_string(),
            summary: "rm -rf *".to_string(),
        },
        UiEvent::ToolPermission {
            allowed: false,
            reason: "通配删除未逐一指定目标".to_string(),
        },
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[工具] "),
            stdout("bash：rm -rf *\n"),
            stdout("[权限] "),
            stdout("拒绝：通配删除未逐一指定目标（工具未执行）\n"),
        ]
    );
}

#[test]
fn tool_result_and_following_answer_start_new_blocks() {
    let chunks = format_all(&[
        UiEvent::ToolCall {
            name: "bash".to_string(),
            summary: "ls".to_string(),
        },
        UiEvent::ToolResult("退出码：0\n".to_string()),
        UiEvent::AnswerText("看完了".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[工具] "),
            stdout("bash：ls\n"),
            stdout("[工具结果] "),
            stdout("退出码：0\n"),
            stdout("[助手] "),
            stdout("看完了"),
        ]
    );
}

#[test]
fn empty_tool_events_do_not_write_anything() {
    let chunks = format_all(&[
        UiEvent::ToolResult(String::new()),
        UiEvent::ToolCall {
            name: "bash".to_string(),
            summary: String::new(),
        },
    ]);

    // 空结果不写；调用摘要为空时仍然写一行工具名。
    assert_eq!(chunks, vec![stdout("[工具] "), stdout("bash\n")]);
}

#[test]
fn reasoning_prefix_is_written_once_per_block() {
    let chunks = format_all(&[
        UiEvent::ReasoningText("先理解".to_string()),
        UiEvent::ReasoningText("问题".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![stdout("[思考] "), stdout("先理解"), stdout("问题"),]
    );
}

#[test]
fn answer_prefix_is_written_once_per_block() {
    let chunks = format_all(&[
        UiEvent::AnswerText("你好".to_string()),
        UiEvent::AnswerText("，我是助手。".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![stdout("[助手] "), stdout("你好"), stdout("，我是助手。"),]
    );
}

#[test]
fn reasoning_to_answer_switches_with_single_newline() {
    let chunks = format_all(&[
        UiEvent::ReasoningText("想".to_string()),
        UiEvent::AnswerText("答".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[思考] "),
            stdout("想"),
            stdout("\n"),
            stdout("[助手] "),
            stdout("答"),
        ]
    );
}

#[test]
fn answer_to_reasoning_switches_with_single_newline() {
    let chunks = format_all(&[
        UiEvent::AnswerText("答".to_string()),
        UiEvent::ReasoningText("再想".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[助手] "),
            stdout("答"),
            stdout("\n"),
            stdout("[思考] "),
            stdout("再想"),
        ]
    );
}

#[test]
fn reasoning_line_break_inside_block_does_not_repeat_prefix() {
    let chunks = format_all(&[
        UiEvent::ReasoningText("第一行\n".to_string()),
        UiEvent::ReasoningText("第二行".to_string()),
        UiEvent::AnswerText("答".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[思考] "),
            stdout("第一行\n"),
            stdout("第二行"),
            stdout("\n"),
            stdout("[助手] "),
            stdout("答"),
        ]
    );
}

#[test]
fn empty_deltas_are_ignored() {
    let chunks = format_all(&[
        UiEvent::ReasoningText(String::new()),
        UiEvent::AnswerText(String::new()),
    ]);

    assert!(chunks.is_empty());
}

#[test]
fn finished_turn_closes_open_line() {
    let chunks = format_all(&[UiEvent::AnswerText("答".to_string()), UiEvent::TurnFinished]);

    assert_eq!(chunks, vec![stdout("[助手] "), stdout("答"), stdout("\n")]);
}

#[test]
fn finished_turn_emits_nothing_for_empty_output() {
    let chunks = format_all(&[UiEvent::TurnFinished]);

    assert!(chunks.is_empty());
}

#[test]
fn finished_turn_does_not_close_a_closed_line_again() {
    let chunks = format_all(&[
        UiEvent::AnswerText("答\n".to_string()),
        UiEvent::TurnFinished,
    ]);

    assert_eq!(chunks, vec![stdout("[助手] "), stdout("答\n")]);
}

#[test]
fn finished_turn_restarts_prefix_in_next_turn() {
    let chunks = format_all(&[
        UiEvent::ReasoningText("想".to_string()),
        UiEvent::TurnFinished,
        UiEvent::ReasoningText("新一轮".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[思考] "),
            stdout("想"),
            stdout("\n"),
            stdout("[思考] "),
            stdout("新一轮"),
        ]
    );
}

#[test]
fn error_closes_open_line_before_message() {
    let chunks = format_all(&[
        UiEvent::ReasoningText("中途".to_string()),
        UiEvent::Error("断流".to_string()),
    ]);

    assert_eq!(
        chunks,
        vec![
            stdout("[思考] "),
            stdout("中途"),
            stdout("\n"),
            stderr("错误：断流\n"),
        ]
    );
}

#[test]
fn farewell_closes_open_line_after_reasoning() {
    let chunks = format_all(&[UiEvent::ReasoningText("想".to_string()), UiEvent::Farewell]);

    assert_eq!(
        chunks,
        vec![
            stdout("[思考] "),
            stdout("想"),
            stdout("\n"),
            stderr("会话结束。\n"),
        ]
    );
}
