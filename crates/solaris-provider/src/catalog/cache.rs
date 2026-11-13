//! Persist raw upstream data; parsing and layering belong to the official catalog.
use super::ModelCatalog;
use rig_core::catalog::Catalog;
use std::{path::Path, time::Duration};
const MAX_BYTES: usize = 32 * 1024 * 1024;
const FILE: &str = "models-dev.json";
fn install(catalog: &ModelCatalog, text: &str) -> Result<(), String> {
    if text.len() > MAX_BYTES {
        return Err("model catalog exceeds 32 MiB".into());
    }
    let (downloaded, _) = Catalog::from_models_dev(text).map_err(|e| e.to_string())?;
    if downloaded.iter().next().is_none() {
        return Err("empty model catalog".into());
    }
    let base = Catalog::builtin().with_models_dev(&downloaded);
    let overrides =
        Catalog::from_overrides(&catalog.overrides, &base).map_err(|e| e.to_string())?;
    *catalog.state.write().unwrap_or_else(|p| p.into_inner()) = base.with_overrides(&overrides);
    Ok(())
}
pub(super) fn load(catalog: &ModelCatalog, directory: &Path) {
    let path = directory.join(FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() <= MAX_BYTES as u64)
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let _ = install(catalog, &text);
    }
}
pub(super) fn start(catalog: ModelCatalog, directory: std::path::PathBuf) {
    tokio::spawn(async move {
        let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
        else {
            return;
        };
        loop {
            let _ = refresh(&catalog, &directory, &client).await;
            tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
        }
    });
}
async fn refresh(
    catalog: &ModelCatalog,
    directory: &Path,
    client: &reqwest::Client,
) -> Result<(), String> {
    let mut response = client
        .get(super::MODELS_DEV_URL)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if body.len().saturating_add(chunk.len()) > MAX_BYTES {
            return Err("model catalog exceeds 32 MiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&body).map_err(|e| e.to_string())?;
    install(catalog, text)?;
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(&body).map_err(|e| e.to_string())?;
    file.persist(directory.join(FILE))
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn data(context: u32) -> String {
        serde_json::json!({"anthropic":{"models":{"solaris-cache-fixture":{
          "name":"Cache fixture","tool_call":true,"reasoning":false,
          "modalities":{"input":["text"],"output":["text"]},
          "limit":{"context":context,"output":4096},
          "cost":{"input":1,"output":2}
        }}}})
        .to_string()
    }
    #[test]
    fn refresh_updates_shared_catalog_but_not_already_resolved_facts() {
        let config = crate::ProviderConfig::default();
        let shared = config.clone();
        install(&config.catalog, &data(32000)).expect("first catalog");
        let before = config.resolve("anthropic", "solaris-cache-fixture");
        install(&config.catalog, &data(64000)).expect("second catalog");
        assert_eq!(
            before.facts.spec().expect("facts").context_window,
            Some(32000)
        );
        assert_eq!(
            shared
                .resolve("anthropic", "solaris-cache-fixture")
                .capabilities
                .context_window,
            Some(64000)
        );
        assert!(install(&config.catalog, "broken JSON").is_err());
        assert!(install(&config.catalog, "{}").is_err());
        assert_eq!(
            shared
                .resolve("anthropic", "solaris-cache-fixture")
                .capabilities
                .context_window,
            Some(64000)
        );
    }
    #[test]
    fn startup_ignores_corrupt_cache_and_applies_official_overrides_last() {
        let config = crate::ProviderConfig::from_json(r#"{"overrides":{"anthropic":{"models":{"claude-sonnet-4-5":{"limit":{"context":12345}}}}}}"#).expect("config");
        let dir = tempfile::tempdir().expect("directory");
        std::fs::write(dir.path().join(FILE), "broken").expect("write");
        load(&config.catalog, dir.path());
        assert_eq!(
            config
                .resolve("anthropic", "claude-sonnet-4-5")
                .capabilities
                .context_window,
            Some(12345)
        );
        std::fs::write(dir.path().join(FILE), data(32000)).expect("write");
        load(&config.catalog, dir.path());
        assert_eq!(
            config
                .resolve("anthropic", "solaris-cache-fixture")
                .capabilities
                .context_window,
            Some(32000)
        );
        assert_eq!(
            config
                .resolve("anthropic", "claude-sonnet-4-5")
                .capabilities
                .context_window,
            Some(12345)
        );
    }
}

#[cfg(test)]
mod reference_tests {
    #[test]
    fn explicit_reference_can_use_a_model_only_present_in_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let raw = r#"{"anthropic":{"models":{"cached-only":{"name":"Cached","tool_call":true,"reasoning":false,"modalities":{"input":["text"],"output":["text"]},"limit":{"context":32000,"output":4096}}}}}"#;
        std::fs::write(dir.path().join(super::FILE), raw).unwrap();
        let config = r#"{"models":{"fast":{"provider":"new-api","request_id":"alias","metadata":"anthropic/cached-only"}}}"#;
        assert!(crate::ProviderConfig::from_json(config).is_err());
        let config = crate::ProviderConfig::from_json_with_cache(config, dir.path()).unwrap();
        assert_eq!(
            config
                .resolve("new-api", "fast")
                .capabilities
                .context_window,
            Some(32000)
        );
    }
}
