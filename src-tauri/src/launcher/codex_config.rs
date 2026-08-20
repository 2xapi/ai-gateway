//! 独立 CODEX_HOME 的 config.toml 生成(env_key 模式,方案 v2 §6.4)。

use std::path::Path;

/// TOML 双引号字符串转义。
fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 写 config.toml(env_key 指向环境变量;key 本身不落盘)。
pub(crate) fn write(
    temp_dir: &Path,
    base_url: &str,
    model: &str,
    wire_api: &str,
) -> Result<(), String> {
    let cfg = format!(
        "model_provider = \"custom\"\nmodel = {}\n\n[model_providers.custom]\nname = \"custom\"\nbase_url = {}\nwire_api = \"{}\"\nenv_key = \"{}\"\nrequires_openai_auth = false\n",
        toml_str(model),
        toml_str(base_url),
        wire_api,
        crate::launcher::ENV_KEY_NAME,
    );
    std::fs::write(temp_dir.join("config.toml"), cfg)
        .map_err(|e| format!("写 config.toml 失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_str_escapes() {
        assert_eq!(toml_str("plain"), "\"plain\"");
        assert_eq!(toml_str("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_str("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn write_produces_expected_config() {
        let dir = std::env::temp_dir().join(format!("codex-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "https://relay.test/v1", "gpt-4o", "chat_completions").unwrap();
        let cfg = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(cfg.contains("model = \"gpt-4o\""));
        assert!(cfg.contains("base_url = \"https://relay.test/v1\""));
        assert!(cfg.contains("wire_api = \"chat_completions\""));
        assert!(cfg.contains(&format!("env_key = \"{}\"", crate::launcher::ENV_KEY_NAME)));
        assert!(cfg.contains("requires_openai_auth = false"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
