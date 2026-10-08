//! 配置文件读取：config.toml 保存模型选择与 provider 连接信息，auth.toml 保存敏感凭据。
//!
//! 这里只负责解析与合并，不校验内容：缺字段留 `None`，由对话时提示用户。
//! 两个文件共用同一套形状，各自只使用自己负责的字段。
//!
//! 启动阶段文件缺失、不可读或无法解析时返回面向用户的文案，由调用方决定终止启动；
//! 原始错误在本模块内记录日志，不再向上传递错误类型。

use std::{collections::BTreeMap, fs, path::Path};

use serde::Deserialize;

/// 非敏感配置文件名，随仓库提交。
const CONFIG_FILE: &str = "config.toml";
/// 敏感凭据文件名，随本地环境提供，不提交。
const AUTH_FILE: &str = "auth.toml";

/// 运行时配置：默认模型规则与合并后的 provider 表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// 默认模型规则，形如 `provider/model`。
    pub default_model: Option<String>,
    /// 各 provider 的连接信息与凭据，键为 provider 名。
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// 单个 provider 的连接信息与凭据，同时是文件里 provider 条目的形状。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProviderConfig {
    /// 服务地址，作为 API base url 使用。
    pub url: Option<String>,
    /// 服务访问凭据。
    pub api_key: Option<String>,
}

impl Config {
    /// 按名字取 provider 配置。
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }
}

/// 读取 `config_dir` 下的两个配置文件并按 provider 名合并。
pub fn load(config_dir: &Path) -> Result<Config, String> {
    let config_path = config_dir.join(CONFIG_FILE);
    let auth_path = config_dir.join(AUTH_FILE);
    ensure_file(&config_path)?;
    ensure_file(&auth_path)?;

    let raw_config: ConfigFile = parse_file(&config_path)?;
    let raw_auth: ConfigFile = parse_file(&auth_path)?;

    // 连接信息只认 config.toml（入库文件），凭据只认 auth.toml（本地文件）；
    // 另一侧出现的同名字段直接忽略，避免凭据写进入库文件后仍被使用。
    let config_llm = raw_config.llm;
    let mut providers: BTreeMap<String, ProviderConfig> = config_llm
        .providers
        .into_iter()
        .map(|(name, entry)| {
            (
                name,
                ProviderConfig {
                    url: entry.url,
                    api_key: None,
                },
            )
        })
        .collect();
    for (name, entry) in raw_auth.llm.providers {
        providers.entry(name).or_default().api_key = entry.api_key;
    }

    Ok(Config {
        default_model: config_llm.default_model,
        providers,
    })
}

/// 检查配置文件存在，缺失时直接给出路径与修复提示。
fn ensure_file(path: &Path) -> Result<(), String> {
    if path.is_file() {
        return Ok(());
    }
    tracing::error!(path = %path.display(), "配置文件不存在");
    Err(format!(
        "配置文件不存在 {}：请在当前工作目录同时提供 {CONFIG_FILE} 与 {AUTH_FILE}",
        path.display()
    ))
}

/// config.toml 与 auth.toml 共用的文件形状：只包含 `[llm]` 一个分节。
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    /// LLM 相关配置；整个分节缺失时按默认值处理。
    #[serde(default)]
    llm: LLMSection,
}

/// 文件里的 `[llm]` 分节：固定骨架 + 动态 provider 表。
#[derive(Debug, Default, Deserialize)]
struct LLMSection {
    /// 默认模型规则，只有 config.toml 会使用。
    default_model: Option<String>,
    /// 各 provider 的条目，键为 provider 名。
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
}

/// 读取并解析单个 TOML 文件。
fn parse_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let content = fs::read_to_string(path).map_err(|error| {
        tracing::error!(%error, path = %path.display(), "读取配置文件失败");
        format!("读取配置文件失败 {}：{error}", path.display())
    })?;
    toml::from_str(&content).map_err(|error| {
        tracing::error!(%error, path = %path.display(), "解析配置文件失败");
        format!("配置文件内容无效 {}：{error}", path.display())
    })
}

#[cfg(test)]
#[path = "../unit_tests/config.rs"]
mod tests;
