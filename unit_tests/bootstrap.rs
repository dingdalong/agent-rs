use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use clap::{Parser, error::ErrorKind};

use super::{LaunchArgs, resolve_workspace};

/// 每个用例独立的临时目录，避免相互干扰。
fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agent-rs-bootstrap-{}-{name}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

#[test]
fn workspace_defaults_to_start_directory() {
    let dir = temp_dir("default");

    let workspace = resolve_workspace(&dir, None).expect("默认工作区应解析成功");

    assert_eq!(workspace, fs::canonicalize(dir).expect("解析默认工作区"));
}

#[test]
fn workspace_accepts_absolute_and_relative_paths() {
    let base = temp_dir("base");
    let absolute = temp_dir("absolute");
    let relative_name = format!("relative-{}", std::process::id());
    let relative = base.join(&relative_name);
    fs::create_dir_all(&relative).expect("创建相对工作区");

    assert_eq!(
        resolve_workspace(&base, Some(absolute.clone())).expect("绝对工作区应解析成功"),
        fs::canonicalize(absolute).expect("解析绝对工作区")
    );
    assert_eq!(
        resolve_workspace(&base, Some(PathBuf::from(relative_name))).expect("相对工作区应解析成功"),
        fs::canonicalize(relative).expect("解析相对工作区")
    );
}

#[test]
fn workspace_rejects_missing_path() {
    let base = temp_dir("missing");
    let missing = base.join("不存在");

    let error = resolve_workspace(&base, Some(missing)).expect_err("不存在的工作区应报错");

    assert!(error.contains("不存在或不可访问"), "{error}");
}

#[test]
fn workspace_rejects_file() {
    let base = temp_dir("file");
    let file = base.join("not-a-directory.txt");
    fs::write(&file, "内容").expect("写入测试文件");

    let error = resolve_workspace(&base, Some(file)).expect_err("文件不能作为工作区");

    assert!(error.contains("不是目录"), "{error}");
}

#[test]
fn arguments_accept_workspace() {
    let args = LaunchArgs::try_parse_from(["agent-rs", "--workspace", "任务工作区"])
        .expect("参数应解析成功");

    assert_eq!(args.workspace, Some(PathBuf::from("任务工作区")));
}

#[test]
fn arguments_reject_unknown_option() {
    let error =
        LaunchArgs::try_parse_from(["agent-rs", "--unknown"]).expect_err("未知参数应被拒绝");

    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn arguments_reject_missing_workspace_value() {
    let error =
        LaunchArgs::try_parse_from(["agent-rs", "--workspace"]).expect_err("缺少工作区应报错");

    assert!(error.to_string().contains("--workspace"), "{error}");
}

#[test]
fn help_is_available() {
    let error =
        LaunchArgs::try_parse_from(["agent-rs", "--help"]).expect_err("帮助应走 clap 的展示路径");

    assert_eq!(error.kind(), ErrorKind::DisplayHelp);
}
