use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
struct TranslationCache {
    entries: HashMap<String, String>,
}

impl TranslationCache {
    fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("create translation cache folder")?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("serialize translation cache")?;
        fs::write(path, bytes).context("write translation cache")
    }
}

/// Translates uncatalogued UI copy through the same Google Translate endpoint
/// used by the original `WinUI` application. Successful results are cached on
/// disk so repeated page loads and offline launches do not repeat requests.
pub fn translate_batch(
    cache_path: &Path,
    language: &str,
    sources: &[&str],
) -> Vec<(String, String)> {
    let target = translation_target(language);
    if target.starts_with("en") {
        return sources
            .iter()
            .map(|source| ((*source).to_owned(), (*source).to_owned()))
            .collect();
    }

    let mut cache = TranslationCache::load(cache_path);
    let mut changed = false;
    let translated = sources
        .iter()
        .map(|source| {
            let key = cache_key(target, source);
            let value = if let Some(value) = cache.entries.get(&key) {
                value.clone()
            } else {
                match download_translation(source, target) {
                    Ok(value) => {
                        cache.entries.insert(key, value.clone());
                        changed = true;
                        value
                    }
                    Err(_) => (*source).to_owned(),
                }
            };
            ((*source).to_owned(), value)
        })
        .collect();
    if changed {
        let _ = cache.save(cache_path);
    }
    translated
}

fn translation_target(language: &str) -> &str {
    match language {
        "zh-Hans" => "zh-CN",
        "zh-Hant" => "zh-TW",
        value => value,
    }
}

fn cache_key(language: &str, source: &str) -> String {
    format!("{language}\n{source}")
}

fn download_translation(source: &str, language: &str) -> anyhow::Result<String> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client", "gtx")
        .append_pair("sl", "en")
        .append_pair("tl", language)
        .append_pair("dt", "t")
        .append_pair("q", source)
        .finish();
    let response = ureq::get(format!(
        "https://translate.googleapis.com/translate_a/single?{query}"
    ))
    .call()
    .context("request UI translation")?;
    let body = response
        .into_body()
        .read_to_string()
        .context("read UI translation")?;
    parse_translation_response(&body)
}

fn parse_translation_response(body: &str) -> anyhow::Result<String> {
    let json: serde_json::Value = serde_json::from_str(body).context("parse translation JSON")?;
    let segments = json
        .get(0)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("translation response has no segments"))?;
    let translated = segments
        .iter()
        .filter_map(|segment| segment.get(0).and_then(serde_json::Value::as_str))
        .collect::<String>();
    if translated.trim().is_empty() {
        Err(anyhow!("translation response was empty"))
    } else {
        Ok(translated)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_all_google_translation_segments() {
        let body =
            r#"[[["Bonjour ","Hello ",null,null,1],["le monde","world",null,null,1]],null,"en"]"#;
        assert_eq!(
            super::parse_translation_response(body).expect("valid response"),
            "Bonjour le monde"
        );
    }

    #[test]
    fn maps_simplified_chinese_to_google_target() {
        assert_eq!(super::translation_target("zh-Hans"), "zh-CN");
        assert_eq!(super::cache_key("fr", "Settings"), "fr\nSettings");
    }
}
