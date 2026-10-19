//! 入口：完成启动准备并驱动交互循环。
//!
//! 启动检查失败时提示并退出；运行期失败只在各模块内提示，不结束进程。

mod agent;
mod bootstrap;
mod config;
mod llm;
mod sandbox;
mod tools;
mod ui;

use crate::{
    agent::Agent,
    ui::{Ui, UiEvent, terminal::TerminalUi},
};

#[tokio::main]
async fn main() {
    bootstrap::init_tracing();
    if let Err(message) = run_session().await {
        eprintln!("错误：{message}");
        std::process::exit(1);
    }
}

/// 启动准备成功后运行会话；运行期失败由各模块提示，不结束进程。
async fn run_session() -> Result<(), String> {
    let context = bootstrap::prepare()?;
    let mut ui = TerminalUi::new();
    let mut agent = Agent::new(context.config, context.workspace);
    while let Some(input) = ui.read_input() {
        agent.chat(&input, &mut ui).await;
    }
    ui.render(UiEvent::Farewell);
    Ok(())
}
