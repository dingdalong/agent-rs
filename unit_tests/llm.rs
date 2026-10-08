use std::collections::BTreeMap;

use super::build;
use crate::config::{Config, ProviderConfig};

/// 构造一个含指定 provider 的配置。
fn config_with(name: &str, url: Option<&str>, api_key: Option<&str>) -> Config {
    let mut providers = BTreeMap::new();
    providers.insert(
        name.to_string(),
        ProviderConfig {
            url: url.map(str::to_string),
            api_key: api_key.map(str::to_string),
        },
    );
    Config {
        default_model: None,
        providers,
    }
}

/// 取出错误消息，便于断言提示内容。
fn error_message(model: &str, config: &Config) -> String {
    build(config, model).err().expect("应构建失败")
}

#[test]
fn build_rejects_model_without_slash() {
    let config = config_with("deepseek", Some("https://api.deepseek.com"), Some("sk"));

    let message = error_message("deepseek-flash", &config);

    assert!(message.contains("provider/model"), "{message}");
}

#[test]
fn build_rejects_empty_model_parts() {
    let config = config_with("deepseek", Some("https://api.deepseek.com"), Some("sk"));

    assert!(error_message("deepseek/", &config).contains("不能为空"));
    assert!(error_message("/deepseek-flash", &config).contains("不能为空"));
}

#[test]
fn build_rejects_unknown_provider() {
    let config = config_with("deepseek", Some("https://api.deepseek.com"), Some("sk"));

    let message = error_message("openai/gpt-5", &config);

    assert!(message.contains("没有 provider"), "{message}");
}

#[test]
fn build_rejects_missing_url() {
    let config = config_with("deepseek", None, Some("sk"));

    let message = error_message("deepseek/deepseek-flash", &config);

    assert!(message.contains("缺少 url"), "{message}");
}

#[test]
fn build_rejects_missing_api_key() {
    let config = config_with("deepseek", Some("https://api.deepseek.com"), None);

    let message = error_message("deepseek/deepseek-flash", &config);

    assert!(message.contains("缺少 api_key"), "{message}");
}

#[test]
fn build_rejects_unimplemented_provider() {
    let config = config_with("openai", Some("https://api.openai.com/v1"), Some("sk"));

    let message = error_message("openai/gpt-5", &config);

    assert!(message.contains("还没有对应实现"), "{message}");
}

#[test]
fn build_returns_deepseek_client_for_valid_spec() {
    let config = config_with("deepseek", Some("https://api.deepseek.com"), Some("sk"));

    let client = build(&config, "deepseek/deepseek-flash");

    assert!(client.is_ok(), "{:?}", client.err());
}
