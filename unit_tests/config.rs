use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use super::{BashConfig, load};
use crate::sandbox::SandboxMode;

/// 每个用例独立的临时目录，避免相互干扰。
fn temp_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agent-rs-config-{}-{name}-{seq}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

/// 写入 config.toml。
fn write_config(dir: &std::path::Path, content: &str) {
    fs::write(dir.join("config.toml"), content).expect("写入 config.toml");
}

/// 写入 auth.toml。
fn write_auth(dir: &std::path::Path, content: &str) {
    fs::write(dir.join("auth.toml"), content).expect("写入 auth.toml");
}

#[test]
fn load_merges_both_files_by_provider_name() {
    let dir = temp_dir("merge");
    write_config(
        &dir,
        r#"
            [llm]
            default_model = "deepseek/deepseek-flash"

            [llm.providers.deepseek]
            url = "https://api.deepseek.com"

            [llm.providers.local]
            url = "http://127.0.0.1:8000"
            "#,
    );
    write_auth(
        &dir,
        r#"
            [llm.providers.deepseek]
            api_key = "sk-deepseek"

            [llm.providers.local]
            api_key = "sk-local"
            "#,
    );

    let config = load(&dir).expect("配置应读取成功");

    assert_eq!(
        config.default_model.as_deref(),
        Some("deepseek/deepseek-flash")
    );
    assert_eq!(config.providers.len(), 2);
    let deepseek = config.provider("deepseek").expect("应有 deepseek");
    assert_eq!(deepseek.url.as_deref(), Some("https://api.deepseek.com"));
    assert_eq!(deepseek.api_key.as_deref(), Some("sk-deepseek"));
    let local = config.provider("local").expect("应有 local");
    assert_eq!(local.url.as_deref(), Some("http://127.0.0.1:8000"));
    assert_eq!(local.api_key.as_deref(), Some("sk-local"));
}

#[test]
fn provider_declared_only_in_config_file_keeps_api_key_none() {
    let dir = temp_dir("url-only");
    write_config(
        &dir,
        "[llm.providers.deepseek]\nurl = \"https://api.deepseek.com\"\n",
    );
    write_auth(&dir, "");

    let config = load(&dir).expect("配置应读取成功");

    let deepseek = config.provider("deepseek").expect("应有 deepseek");
    assert_eq!(deepseek.url.as_deref(), Some("https://api.deepseek.com"));
    assert!(deepseek.api_key.is_none());
}

#[test]
fn provider_declared_only_in_auth_file_keeps_url_none() {
    let dir = temp_dir("key-only");
    write_config(&dir, "");
    write_auth(&dir, "[llm.providers.deepseek]\napi_key = \"sk-test\"\n");

    let config = load(&dir).expect("配置应读取成功");

    let deepseek = config.provider("deepseek").expect("应有 deepseek");
    assert!(deepseek.url.is_none());
    assert_eq!(deepseek.api_key.as_deref(), Some("sk-test"));
}

#[test]
fn config_file_api_key_is_ignored() {
    let dir = temp_dir("config-key-ignored");
    write_config(
        &dir,
        "[llm.providers.deepseek]\nurl = \"https://api.deepseek.com\"\napi_key = \"sk-config\"\n",
    );
    write_auth(&dir, "[llm.providers.deepseek]\napi_key = \"sk-auth\"\n");

    let config = load(&dir).expect("配置应读取成功");

    let deepseek = config.provider("deepseek").expect("应有 deepseek");
    assert_eq!(deepseek.api_key.as_deref(), Some("sk-auth"));
}

#[test]
fn config_file_api_key_does_not_fill_missing_auth_key() {
    let dir = temp_dir("config-key-only");
    write_config(
        &dir,
        "[llm.providers.deepseek]\nurl = \"https://api.deepseek.com\"\napi_key = \"sk-config\"\n",
    );
    write_auth(&dir, "");

    let config = load(&dir).expect("配置应读取成功");

    let deepseek = config.provider("deepseek").expect("应有 deepseek");
    assert!(deepseek.api_key.is_none(), "入库文件里的 api_key 不应生效");
}

#[test]
fn auth_file_url_is_ignored() {
    let dir = temp_dir("auth-url-ignored");
    write_config(&dir, "");
    write_auth(
        &dir,
        "[llm.providers.deepseek]\nurl = \"https://evil.example.com\"\napi_key = \"sk-auth\"\n",
    );

    let config = load(&dir).expect("配置应读取成功");

    let deepseek = config.provider("deepseek").expect("应有 deepseek");
    assert!(deepseek.url.is_none(), "auth.toml 里的 url 不应生效");
    assert_eq!(deepseek.api_key.as_deref(), Some("sk-auth"));
}

#[test]
fn missing_llm_section_is_not_an_error() {
    let dir = temp_dir("no-llm");
    write_config(&dir, "\"其他\" = \"未知分节会被忽略\"\n");
    write_auth(&dir, "");

    let config = load(&dir).expect("空配置也应读取成功");

    assert_eq!(config.default_model, None);
    assert!(config.providers.is_empty());
}

#[test]
fn invalid_toml_reports_parse_error() {
    let dir = temp_dir("bad-toml");
    write_config(&dir, "this is not = toml\n");
    write_auth(&dir, "");

    let error = load(&dir).expect_err("语法错误应报错");

    assert!(error.contains("内容无效"), "{error}");
    assert!(error.contains("config.toml"), "{error}");
}

#[test]
fn load_reports_missing_auth_file() {
    let dir = temp_dir("no-auth");
    write_config(&dir, "");

    let error = load(&dir).expect_err("缺少 auth.toml 应报错");

    assert!(error.contains("auth.toml"), "{error}");
}

#[test]
fn load_reports_missing_config_file() {
    let dir = temp_dir("no-config");
    write_auth(&dir, "");

    let error = load(&dir).expect_err("缺少 config.toml 应报错");

    assert!(error.contains("config.toml"), "{error}");
}

#[test]
fn bash_tool_section_is_loaded() {
    let dir = temp_dir("tools-bash");
    write_config(
        &dir,
        r#"
            [tools.bash]
            sandbox_mode = "read-only"
            network_access = true
            timeout_ms = 5000
            writable_roots = ["/opt/数据"]
            "#,
    );
    write_auth(&dir, "");

    let config = load(&dir).expect("配置应读取成功");

    assert_eq!(config.bash.sandbox_mode, SandboxMode::ReadOnly);
    assert!(config.bash.network_access);
    assert_eq!(config.bash.timeout_ms, 5_000);
    assert_eq!(config.bash.writable_roots, vec![PathBuf::from("/opt/数据")]);
}

#[test]
fn bash_tool_defaults_are_conservative() {
    let dir = temp_dir("tools-default");
    write_config(&dir, "");
    write_auth(&dir, "");

    let config = load(&dir).expect("配置应读取成功");

    assert_eq!(config.bash, BashConfig::default());
    assert_eq!(config.bash.sandbox_mode, SandboxMode::WorkspaceWrite);
    assert!(!config.bash.network_access, "默认必须禁网");
    assert_eq!(config.bash.timeout_ms, 120_000);
    assert!(config.bash.writable_roots.is_empty());
}

#[test]
fn bash_tool_section_rejects_unknown_field() {
    let dir = temp_dir("tools-unknown-field");
    write_config(&dir, "[tools.bash]\nnetwrok_access = true\n");
    write_auth(&dir, "");

    let error = load(&dir).expect_err("字段名写错应报错");

    assert!(error.contains("内容无效"), "{error}");
}

#[test]
fn bash_tool_section_rejects_unknown_mode() {
    let dir = temp_dir("tools-unknown-mode");
    write_config(&dir, "[tools.bash]\nsandbox_mode = \"无限制\"\n");
    write_auth(&dir, "");

    let error = load(&dir).expect_err("未知沙盒模式应报错");

    assert!(error.contains("内容无效"), "{error}");
}
