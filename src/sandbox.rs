//! 沙盒层：把「要执行的命令」包装成平台沙盒命令，限制它能写哪些目录、能不能联网。
//!
//! - macOS 用系统 Seatbelt（固定 `/usr/bin/sandbox-exec`，不经 PATH），路径经 `-D` 参数传入
//!   profile，profile 里只写参数名，不做字符串拼接；
//! - Linux 用 bubblewrap（`bwrap`），靠只读根挂载 + 可写根绑定 + 命名空间隔离实现同样的语义；
//! - 平台沙盒无法落实时直接失败，绝不改成「无沙盒执行」（fail-closed）。
//!
//! 本模块只负责「策略 → 命令行」，执行、超时与输出整理在工具层。

use std::path::{Path, PathBuf};

use std::fmt::Write as _;

use serde::Deserialize;
use tokio::process::Command;

/// 当前平台使用的沙盒实现名，用于日志与环境变量标记。
pub fn kind() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "seatbelt"
    }
    #[cfg(target_os = "linux")]
    {
        "bwrap"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "none"
    }
}

/// 所有平台统一使用的 shell。
const SHELL_PATH: &str = "/bin/bash";
/// 标记命令处于哪个沙盒实现下，便于脚本与测试观测。
const SANDBOX_ENV: &str = "AGENT_SANDBOX";
/// 沙盒禁网时置为 1。
const SANDBOX_NETWORK_DISABLED_ENV: &str = "AGENT_SANDBOX_NETWORK_DISABLED";

/// macOS 的 Seatbelt 工具路径；走固定路径避免被 PATH 上的同名程序顶替。
#[cfg(target_os = "macos")]
const SEATBELT_PATH: &str = "/usr/bin/sandbox-exec";

/// Linux 的 bubblewrap 可执行名。
#[cfg(target_os = "linux")]
const BWRAP_PROGRAM: &str = "bwrap";

/// Linux 上 bubblewrap 缺失时的提示。
#[cfg(target_os = "linux")]
const MISSING_BWRAP_MESSAGE: &str = "Linux 沙盒需要 bubblewrap：请先安装（Ubuntu/Debian: sudo apt install bubblewrap；Fedora: sudo dnf install bubblewrap）";

/// Linux 探测 bubblewrap 可用性的等待上限。
#[cfg(target_os = "linux")]
const BWRAP_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// 已知的用户命名空间受限特征（不同发行版/容器环境的报错文案）。
#[cfg(target_os = "linux")]
const USER_NAMESPACE_FAILURES: [&str; 4] = [
    "loopback: Failed RTM_NEWADDR",
    "loopback: Failed RTM_NEWLINK",
    "setting up uid map: Permission denied",
    "No permissions to create a new namespace",
];

/// macOS 联网时追加的策略：只放行外发/入境连接与系统网络服务所需的 Mach 服务。
///
/// 只放行出/入站连接，不放行本地端口监听；DNS 与 TLS 依赖的系统服务单独列出。
#[cfg(target_os = "macos")]
const SEATBELT_NETWORK_POLICY: &str = concat!(
    "; 网络：放行外发与入境连接，以及 DNS/证书/网络配置相关的系统服务\n",
    "(allow network-outbound)\n",
    "(allow network-inbound)\n",
    "(allow system-socket\n",
    "  (require-all\n",
    "    (socket-domain AF_SYSTEM)\n",
    "    (socket-protocol 2)))\n",
    "(allow mach-lookup\n",
    "  (global-name \"com.apple.bsd.dirhelper\")\n",
    "  (global-name \"com.apple.system.opendirectoryd.membership\")\n",
    "  (global-name \"com.apple.SecurityServer\")\n",
    "  (global-name \"com.apple.networkd\")\n",
    "  (global-name \"com.apple.ocspd\")\n",
    "  (global-name \"com.apple.trustd.agent\")\n",
    "  (global-name \"com.apple.SystemConfiguration.DNSConfiguration\")\n",
    "  (global-name \"com.apple.SystemConfiguration.configd\"))\n",
);

/// macOS 基础策略：默认拒绝，只放行跑普通开发命令必需的能力。
///
/// 进程与信号、进程信息查询、`/dev/null` 写入、sysctl 读取、IOKit 根域、
/// 用户信息查询的 Mach 服务，以及 Python 多进程/OpenMP 用到的 POSIX 信号量与共享内存。
#[cfg(target_os = "macos")]
const SEATBELT_BASE_POLICY: &str = concat!(
    "(version 1)\n",
    "; 默认拒绝，只放行下面显式列出的能力\n",
    "(deny default)\n",
    "(allow process-exec)\n",
    "(allow process-fork)\n",
    "(allow signal (target same-sandbox))\n",
    "(allow process-info* (target same-sandbox))\n",
    "(allow file-write-data\n",
    "  (require-all\n",
    "    (path \"/dev/null\")\n",
    "    (vnode-type CHARACTER-DEVICE)))\n",
    "(allow sysctl-read)\n",
    "(allow iokit-open\n",
    "  (iokit-registry-entry-class \"RootDomainUserClient\"))\n",
    "(allow mach-lookup\n",
    "  (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n",
    "(allow ipc-posix-sem)\n",
    "(allow ipc-posix-shm-read-data\n",
    "  ipc-posix-shm-write-create\n",
    "  ipc-posix-shm-write-unlink\n",
    "  (ipc-posix-name-regex #\"^/__KMP_REGISTERED_LIB_[0-9]+$\"))\n",
    "; 读：整机只读\n",
    "(allow file-read*)\n",
);

/// 沙盒模式：决定命令能写到哪里。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// 工作区与临时目录可写，系统其余部分只读。
    #[default]
    WorkspaceWrite,
    /// 任何目录都不可写。
    ReadOnly,
}

/// 沙盒策略：限定命令可写的目录与能否访问网络。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// 沙盒模式。
    pub mode: SandboxMode,
    /// 是否允许命令访问网络。
    pub network_access: bool,
    /// 工作目录，同时也是默认可写的项目根。
    pub workspace: PathBuf,
    /// 配置里追加的可写根（工作区与临时目录之外的目录）。
    pub writable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// 用配置项构造策略。
    pub fn new(
        mode: SandboxMode,
        network_access: bool,
        workspace: PathBuf,
        writable_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            mode,
            network_access,
            workspace,
            writable_roots,
        }
    }

    /// 实际可写根：工作区 + 系统临时目录 + 配置追加项。
    ///
    /// 解析成真实路径（符号链接按目标算）、去掉重复项、忽略不存在的目录——
    /// 不存在的目录无法挂载/授权，缺失就在这里显式排除，而不是留给沙盒报错。
    pub fn effective_writable_roots(&self) -> Vec<PathBuf> {
        if self.mode == SandboxMode::ReadOnly {
            return Vec::new();
        }
        let mut roots = Vec::new();
        for candidate in std::iter::once(&self.workspace)
            .chain([&std::env::temp_dir(), &PathBuf::from("/tmp")])
            .chain(self.writable_roots.iter())
        {
            let Some(root) = canonical_dir(candidate) else {
                continue;
            };
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        roots
    }

    /// 探测平台沙盒是否可用；不可用时返回面向用户的说明。
    ///
    /// 探测失败不会阻止程序启动，但对应命令不会被执行（fail-closed）。
    pub fn probe(&self) -> Result<(), String> {
        // 相对路径既做不了挂载点，也写不进策略参数，先在这里挡住。
        if !self.workspace.is_absolute() {
            return Err(format!(
                "工作目录必须是绝对路径，当前是 {}",
                self.workspace.display()
            ));
        }
        #[cfg(target_os = "macos")]
        {
            seatbelt_path().map(|_| ())
        }
        #[cfg(target_os = "linux")]
        {
            let bwrap = self.find_bwrap()?;
            self.probe_bwrap(&bwrap)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Err("bash 工具暂不支持当前系统：沙盒只在 macOS 与 Linux 上实现".to_string())
        }
    }

    /// 把一条命令包装成平台沙盒命令；`cwd` 是命令的工作目录。
    pub fn wrap(&self, command: &str, cwd: &Path) -> Result<Command, String> {
        #[cfg(target_os = "macos")]
        {
            let mut wrapped = Command::new(seatbelt_path()?);
            wrapped.args(self.seatbelt_args(command));
            wrapped.current_dir(cwd);
            self.mark_environment(&mut wrapped);
            Ok(wrapped)
        }
        #[cfg(target_os = "linux")]
        {
            let mut wrapped = Command::new(self.find_bwrap()?);
            wrapped.args(self.bwrap_args(command, cwd));
            wrapped.current_dir(cwd);
            self.mark_environment(&mut wrapped);
            Ok(wrapped)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (command, cwd);
            Err("bash 工具暂不支持当前系统：沙盒只在 macOS 与 Linux 上实现".to_string())
        }
    }

    /// 给子进程打上沙盒标记：脚本和测试据此判断自己是否在沙盒里。
    fn mark_environment(&self, command: &mut Command) {
        command.env(SANDBOX_ENV, kind());
        if !self.network_access {
            command.env(SANDBOX_NETWORK_DISABLED_ENV, "1");
        }
    }

    /// Seatbelt 策略文本；路径以 `-D` 参数传入，用参数名引用。
    #[cfg(target_os = "macos")]
    fn seatbelt_profile(&self) -> String {
        let mut profile = String::from(SEATBELT_BASE_POLICY);
        let roots = self.effective_writable_roots();
        if !roots.is_empty() {
            profile.push_str("; 写：只放行工作区、临时目录与配置追加的可写根\n");
            profile.push_str("(allow file-write*\n");
            for index in 0..roots.len() {
                let _ = writeln!(profile, "  (subpath (param \"WRITABLE_ROOT_{index}\"))");
            }
            profile.push_str("  (subpath \"/dev/fd\")\n");
            profile.push_str("  (literal \"/dev/null\"))\n");
            profile.push_str("; 可写根本身不允许改名或删除，否则后续命令的策略会指向别处\n");
            for index in 0..roots.len() {
                let _ = writeln!(
                    profile,
                    "(deny file-write-unlink\n  (require-all\n    (literal (param \"WRITABLE_ROOT_{index}\"))\n    (vnode-type DIRECTORY)))"
                );
            }
        }
        if self.network_access {
            profile.push_str(SEATBELT_NETWORK_POLICY);
        }
        profile
    }

    /// `sandbox-exec` 的完整参数：策略 + 路径参数 + 要执行的命令。
    #[cfg(target_os = "macos")]
    fn seatbelt_args(&self, command: &str) -> Vec<String> {
        let mut args = vec!["-p".to_string(), self.seatbelt_profile()];
        for (index, root) in self.effective_writable_roots().iter().enumerate() {
            args.push(format!(
                "-DWRITABLE_ROOT_{index}={}",
                root.to_string_lossy()
            ));
        }
        args.push("--".to_string());
        args.push(SHELL_PATH.to_string());
        args.push("-c".to_string());
        args.push(command.to_string());
        args
    }

    /// `bwrap` 的完整参数：只读根挂载 + 可写根绑定 + 命名空间隔离 + 命令。
    #[cfg(target_os = "linux")]
    fn bwrap_args(&self, command: &str, cwd: &Path) -> Vec<String> {
        let mut args: Vec<String> = [
            "--new-session",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--bind-try",
            "/dev/shm",
            "/dev/shm",
        ]
        .iter()
        .map(|value| value.to_string())
        .collect();
        for root in self.effective_writable_roots() {
            let path = root.to_string_lossy().to_string();
            args.push("--bind".to_string());
            args.push(path.clone());
            args.push(path);
        }
        // 用户/进程/IPC 命名空间：隔离进程视图，沙盒被终止时整组一起消失。
        args.push("--unshare-user".to_string());
        args.push("--unshare-pid".to_string());
        args.push("--unshare-ipc".to_string());
        if !self.network_access {
            args.push("--unshare-net".to_string());
        }
        args.push("--proc".to_string());
        args.push("/proc".to_string());
        args.push("--cap-drop".to_string());
        args.push("ALL".to_string());
        let cwd = canonical_dir(cwd).unwrap_or_else(|| cwd.to_path_buf());
        args.push("--chdir".to_string());
        args.push(cwd.to_string_lossy().to_string());
        args.push("--".to_string());
        args.push(SHELL_PATH.to_string());
        args.push("-c".to_string());
        args.push(command.to_string());
        args
    }

    /// 在 PATH 上找 bubblewrap；位于可写根（模型可能替换）的候选直接跳过。
    #[cfg(target_os = "linux")]
    fn find_bwrap(&self) -> Result<PathBuf, String> {
        let path_var = std::env::var_os("PATH")
            .ok_or_else(|| format!("{MISSING_BWRAP_MESSAGE}（环境变量 PATH 缺失）"))?;
        let search_paths: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
        find_bwrap_in(&search_paths, &self.effective_writable_roots())
            .ok_or_else(|| MISSING_BWRAP_MESSAGE.to_string())
    }

    /// 跑一次最小沙盒命令，确认能创建用户命名空间。
    #[cfg(target_os = "linux")]
    fn probe_bwrap(&self, bwrap: &Path) -> Result<(), String> {
        use std::process::Stdio;

        let mut child = std::process::Command::new(bwrap)
            .args([
                "--unshare-user",
                "--unshare-net",
                "--ro-bind",
                "/",
                "/",
                "/bin/true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                tracing::error!(%error, path = %bwrap.display(), "启动 bubblewrap 失败");
                format!("启动 bubblewrap 失败（{}）：{error}", bwrap.display())
            })?;

        let deadline = std::time::Instant::now() + BWRAP_PROBE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        // 探测超时按「可用」处理：真正的失败会由被包装的命令自己报出来。
                        let _ = child.kill();
                        let _ = child.wait();
                        tracing::warn!(path = %bwrap.display(), "bubblewrap 探测超时，按可用处理");
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(%error, path = %bwrap.display(), "bubblewrap 探测失败，按可用处理");
                    return Ok(());
                }
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|error| format!("读取 bubblewrap 探测结果失败：{error}"))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if USER_NAMESPACE_FAILURES
            .iter()
            .any(|failure| stderr.contains(failure))
        {
            tracing::error!(path = %bwrap.display(), %stderr, "bubblewrap 无法创建用户命名空间");
            return Err(
                "bubblewrap 无法创建用户命名空间：请检查内核的非特权用户命名空间开关或 AppArmor 限制（Ubuntu 24.04 可加载 bwrap-userns-restrict 配置）"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// 在给定搜索路径里找可用的 bwrap；`forbidden_roots` 内的候选会被跳过。
#[cfg(target_os = "linux")]
fn find_bwrap_in(search_paths: &[PathBuf], forbidden_roots: &[PathBuf]) -> Option<PathBuf> {
    search_paths.iter().find_map(|dir| {
        let candidate = canonical_executable(&dir.join(BWRAP_PROGRAM))?;
        if forbidden_roots
            .iter()
            .any(|root| candidate.starts_with(root))
        {
            tracing::warn!(path = %candidate.display(), "跳过位于可写目录内的 bubblewrap");
            return None;
        }
        Some(candidate)
    })
}

/// 解析成可执行文件的真实路径；不可执行或不存在时返回 `None`。
#[cfg(target_os = "linux")]
fn canonical_executable(candidate: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(candidate).ok()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }
    std::fs::canonicalize(candidate).ok()
}

/// 解析成真实目录路径；路径不存在或不是目录时返回 `None`。
fn canonical_dir(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    canonical.is_dir().then_some(canonical)
}

/// macOS 的 Seatbelt 可执行路径；缺失时给出面向用户的说明。
#[cfg(target_os = "macos")]
fn seatbelt_path() -> Result<&'static Path, String> {
    let path = Path::new(SEATBELT_PATH);
    if path.is_file() {
        return Ok(path);
    }
    Err(format!(
        "macOS 沙盒不可用：找不到 {SEATBELT_PATH}，为避免无沙盒执行，命令已拒绝"
    ))
}

#[cfg(test)]
#[path = "../unit_tests/sandbox.rs"]
mod tests;
