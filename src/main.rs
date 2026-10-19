//! 入口：初始化日志与配置，驱动交互循环。
//!
//! 启动检查失败时提示并退出；运行期失败只在各模块内提示，不结束进程。

mod agent;
mod config;
mod llm;
mod sandbox;
mod tools;
mod ui;

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

use crate::{
    agent::Agent,
    ui::{Ui, UiEvent, terminal::TerminalUi},
};

#[tokio::main]
async fn main() {
    init_tracing();

    let workspace = match workspace_dir() {
        Ok(workspace) => workspace,
        Err(message) => {
            eprintln!("错误：{message}");
            std::process::exit(1);
        }
    };
    let config = match config::load(&workspace) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("错误：{message}");
            std::process::exit(1);
        }
    };
    let provider_names = config
        .providers
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join("、");
    tracing::info!(
        default_model = %config.default_model.as_deref().unwrap_or("未配置"),
        providers = %provider_names,
        workspace = %workspace.display(),
        "配置加载完成"
    );

    let mut ui = TerminalUi::new();
    let mut agent = Agent::new(config, workspace);
    while let Some(input) = ui.read_input() {
        agent.chat(&input, &mut ui).await;
    }
    ui.render(UiEvent::Farewell);
}

/// 当前工作目录：既是配置文件所在目录，也是命令与沙盒的默认根。
fn workspace_dir() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|error| {
        tracing::error!(%error, "获取当前工作目录失败");
        format!("获取当前工作目录失败：{error}")
    })
}

/// 初始化日志：默认只显示警告与错误，可用 `RUST_LOG` 覆盖。
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
