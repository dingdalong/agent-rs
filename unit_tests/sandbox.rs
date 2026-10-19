use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use super::{SandboxMode, SandboxPolicy, kind};

/// 每个用例独立的临时目录，避免相互干扰。
fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agent-rs-sandbox-{}-{name}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

/// 以临时目录为工作区构造策略。
fn policy(name: &str, mode: SandboxMode, network_access: bool) -> SandboxPolicy {
    SandboxPolicy::new(mode, network_access, temp_dir(name), Vec::new())
}

#[test]
fn effective_writable_roots_resolve_symlinks_and_skip_missing_paths() {
    let target = temp_dir("linked-target");
    let link = temp_dir("link-holder").join("link");
    std::os::unix::fs::symlink(&target, &link).expect("创建符号链接");
    let missing = temp_dir("missing-holder").join("还不存在");
    let mut policy = policy("roots", SandboxMode::WorkspaceWrite, false);
    policy.writable_roots = vec![link.clone(), missing.clone()];

    let roots = policy.effective_writable_roots();

    let canonical_target = fs::canonicalize(&target).expect("解析目标目录");
    assert!(roots.contains(&canonical_target), "{roots:?}");
    assert!(
        !roots.contains(&link),
        "符号链接本身不应留在可写根里：{roots:?}"
    );
    assert!(
        !roots.contains(&missing),
        "不存在的目录不应留在可写根里：{roots:?}"
    );
}

#[test]
fn read_only_mode_has_no_writable_root() {
    let mut policy = policy("read-only", SandboxMode::ReadOnly, false);
    policy.writable_roots = vec![temp_dir("read-only-extra")];

    assert!(policy.effective_writable_roots().is_empty());
}

#[test]
fn kind_matches_current_platform() {
    #[cfg(target_os = "macos")]
    assert_eq!(kind(), "seatbelt");
    #[cfg(target_os = "linux")]
    assert_eq!(kind(), "bwrap");
}

#[test]
fn probe_rejects_relative_workspace() {
    let policy = SandboxPolicy::new(
        SandboxMode::WorkspaceWrite,
        false,
        PathBuf::from("相对路径"),
        Vec::new(),
    );

    let error = policy.probe().expect_err("相对工作目录应报错");

    assert!(error.contains("绝对路径"), "{error}");
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_profile_grants_writes_only_through_params() {
    let policy = policy("seatbelt-write", SandboxMode::WorkspaceWrite, false);

    let profile = policy.seatbelt_profile();

    assert!(profile.starts_with("(version 1)"));
    assert!(profile.contains("(deny default)"));
    assert!(profile.contains("(allow process-info* (target same-sandbox))"));
    assert!(profile.contains("(allow file-read*)"));
    assert!(profile.contains("(allow file-write*"));
    assert!(profile.contains("(subpath (param \"WRITABLE_ROOT_0\"))"));
    assert!(profile.contains("(deny file-write-unlink"));
    // 默认禁网：不带任何联网放行规则。
    assert!(!profile.contains("network-outbound"));
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_network_access_appends_system_service_rules() {
    let policy = policy("seatbelt-network", SandboxMode::WorkspaceWrite, true);

    let profile = policy.seatbelt_profile();

    assert!(profile.contains("(allow network-outbound)"));
    assert!(profile.contains("(allow network-inbound)"));
    assert!(profile.contains("com.apple.networkd"));
    assert!(profile.contains("com.apple.SystemConfiguration.DNSConfiguration"));
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_read_only_profile_has_no_write_rules() {
    let policy = policy("seatbelt-read-only", SandboxMode::ReadOnly, false);

    let profile = policy.seatbelt_profile();

    assert!(!profile.contains("WRITABLE_ROOT"));
    assert!(!profile.contains("(allow file-write*"));
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_args_pass_every_writable_root_as_parameter() {
    let policy = policy("seatbelt-args", SandboxMode::WorkspaceWrite, false);

    let args = policy.seatbelt_args("echo hi");
    let workspace = fs::canonicalize(&policy.workspace).expect("解析工作区");

    assert_eq!(args[0], "-p");
    assert!(args[1].contains("(version 1)"));
    let parameters: Vec<&String> = args
        .iter()
        .filter(|arg| arg.starts_with("-DWRITABLE_ROOT_"))
        .collect();
    assert_eq!(parameters.len(), policy.effective_writable_roots().len());
    assert!(
        parameters
            .iter()
            .any(|parameter| *parameter == &format!("-DWRITABLE_ROOT_0={}", workspace.display())),
        "{parameters:?}"
    );
    assert_eq!(
        &args[args.len() - 4..],
        ["--", "/bin/bash", "-c", "echo hi"]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_probe_reports_available() {
    let policy = policy("seatbelt-probe", SandboxMode::WorkspaceWrite, false);

    assert!(policy.probe().is_ok(), "本机应能找到 /usr/bin/sandbox-exec");
}

#[cfg(target_os = "linux")]
use super::find_bwrap_in;

#[cfg(target_os = "linux")]
#[test]
fn bwrap_args_isolate_filesystem_processes_and_network() {
    let policy = policy("bwrap-args", SandboxMode::WorkspaceWrite, false);
    let workspace = fs::canonicalize(&policy.workspace).expect("解析工作区");

    let args = policy.bwrap_args("echo hi", &workspace);

    let windows: Vec<Vec<String>> = args.windows(2).map(|pair| pair.to_vec()).collect();
    assert!(windows.contains(&vec!["--ro-bind".to_string(), "/".to_string()]));
    assert!(windows.contains(&vec![
        "--new-session".to_string(),
        "--die-with-parent".to_string()
    ]));
    assert!(windows.contains(&vec![
        "--unshare-user".to_string(),
        "--unshare-pid".to_string()
    ]));
    assert!(windows.contains(&vec![
        "--unshare-ipc".to_string(),
        "--unshare-net".to_string()
    ]));
    assert!(windows.contains(&vec!["--cap-drop".to_string(), "ALL".to_string()]));
    assert_eq!(
        &args[args.len() - 4..],
        ["--", "/bin/bash", "-c", "echo hi"]
    );
    assert!(
        args.windows(3).any(|window| window
            == [
                "--bind".to_string(),
                workspace.display().to_string(),
                workspace.display().to_string()
            ]),
        "{args:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_network_access_keeps_the_network_namespace() {
    let policy = policy("bwrap-network", SandboxMode::WorkspaceWrite, true);

    let args = policy.bwrap_args("true", &policy.workspace);

    assert!(!args.iter().any(|arg| arg == "--unshare-net"));
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_lookup_skips_candidates_inside_writable_roots() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("bwrap-path");
    let candidate = dir.join("bwrap");
    fs::write(&candidate, "#!/bin/sh\n").expect("写入假 bwrap");
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).expect("设置为可执行");
    let search_paths = vec![dir.clone()];
    let canonical = fs::canonicalize(&dir).expect("解析目录");

    assert_eq!(find_bwrap_in(&search_paths, &[]), Some(candidate));
    assert_eq!(find_bwrap_in(&search_paths, &[canonical]), None);
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_lookup_reports_missing_program() {
    assert_eq!(find_bwrap_in(&[], &[]), None);
}
