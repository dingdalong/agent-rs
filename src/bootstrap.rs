//! 启动流程：初始化日志，解析启动参数，确定配置目录与工具工作区，再加载配置。
//!
//! 配置文件始终从启动目录读取；工具与沙盒可以使用独立工作区。工作区必须是
//! 可访问的真实目录，相对路径以启动目录为基准解析，最终统一成绝对路径。

use std::{
    fs,
    path::{Path, PathBuf},
};

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::{self, Config};

/// 命令行参数。
#[derive(Debug, Parser)]
#[command(name = "agent-rs", version, about = "运行交互式 Agent 会话")]
struct LaunchArgs {
    /// 工具执行与沙盒使用的工作区；配置文件仍从启动目录读取
    #[arg(long, value_name = "目录")]
    workspace: Option<PathBuf>,
}

/// 启动完成后交给会话循环的配置与工作区。
pub struct LaunchContext {
    /// 已加载的运行配置。
    pub config: Config,
    /// 规范化后的工具与沙盒工作区。
    pub workspace: PathBuf,
}

/// 初始化日志：默认只显示警告与错误，可用 `RUST_LOG` 覆盖。
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// 完成启动准备；失败信息面向用户，并保留必要的错误日志。
pub fn prepare() -> Result<LaunchContext, String> {
    let args = LaunchArgs::parse();
    let config_dir = current_dir()?;
    let workspace = resolve_workspace(&config_dir, args.workspace)?;
    let config = config::load(&config_dir)?;
    tracing::info!(
        config_dir = %config_dir.display(),
        workspace = %workspace.display(),
        default_model = %config.default_model.as_deref().unwrap_or("未配置"),
        "启动环境就绪"
    );
    Ok(LaunchContext { config, workspace })
}

/// 启动目录：配置文件从这里读取，也是未指定工作区时的默认根。
fn current_dir() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|error| {
        tracing::error!(%error, "获取当前工作目录失败");
        format!("获取当前工作目录失败：{error}")
    })
}

/// 解析并校验工作区；未指定时沿用启动目录。
fn resolve_workspace(base_dir: &Path, requested: Option<PathBuf>) -> Result<PathBuf, String> {
    let candidate = match requested {
        Some(path) if path.is_absolute() => path,
        Some(path) => base_dir.join(path),
        None => base_dir.to_path_buf(),
    };
    let workspace = fs::canonicalize(&candidate).map_err(|error| {
        tracing::error!(
            %error,
            path = %candidate.display(),
            "工作区不存在或不可访问"
        );
        format!("工作区不存在或不可访问 {}：{error}", candidate.display())
    })?;
    if !workspace.is_dir() {
        tracing::error!(path = %workspace.display(), "工作区不是目录");
        return Err(format!("工作区不是目录：{}", workspace.display()));
    }
    Ok(workspace)
}

#[cfg(test)]
#[path = "../unit_tests/bootstrap.rs"]
mod tests;
