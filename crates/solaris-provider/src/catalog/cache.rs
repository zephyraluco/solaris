//! Independent caches keep one failed upstream from discarding the other.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    CatalogSnapshot, MODELS_DEV_URL, ModelCatalog, OPENROUTER_URL, parse_models_dev,
    parse_openrouter,
};

pub(super) const MAX_BYTES: usize = 32 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
const SOURCES: [(&str, &str); 2] = [
    ("models.dev", MODELS_DEV_URL),
    ("openrouter", OPENROUTER_URL),
];

#[derive(Serialize, Deserialize)]
struct Cached {
    version: u32,
    source: String,
    updated_at: u64,
    checksum: String,
    snapshot: CatalogSnapshot,
}
fn digest(snapshot: &CatalogSnapshot) -> Result<String, String> {
    let data = serde_json::to_vec(snapshot).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(data)))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn path(directory: &Path, source: &str) -> PathBuf {
    directory.join(format!("catalog-{source}-v1.json"))
}
fn read(directory: &Path, source: &str) -> Option<Cached> {
    let path = path(directory, source);
    if std::fs::metadata(&path).ok()?.len() > MAX_BYTES as u64 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let cache: Cached = serde_json::from_str(&text).ok()?;
    if cache.version != 1
        || cache.source != source
        || cache.updated_at > now()
        || digest(&cache.snapshot).ok()? != cache.checksum
    {
        return None;
    }
    CatalogSnapshot::from_json(&serde_json::to_string(&cache.snapshot).ok()?).ok()?;
    Some(cache)
}
fn install(catalog: &ModelCatalog, source: &str, snapshot: CatalogSnapshot) {
    catalog
        .state
        .0
        .write()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(format!("cache:{source}"), snapshot);
}
pub(super) fn load(catalog: &ModelCatalog, directory: &Path) {
    for (source, _) in SOURCES {
        if let Some(cache) = read(directory, source) {
            install(catalog, source, cache.snapshot);
        }
    }
}

async fn download(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_BYTES as u64)
    {
        return Err("model directory is too large".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            return Err("model directory is too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|error| error.to_string())
}
async fn refresh(
    catalog: &ModelCatalog,
    directory: &Path,
    client: &reqwest::Client,
    source: &str,
    url: &str,
) -> Result<(), String> {
    if read(directory, source)
        .is_some_and(|cache| now().saturating_sub(cache.updated_at) < TTL.as_secs())
    {
        return Ok(());
    }
    let text = download(client, url).await?;
    let snapshot = if source == "openrouter" {
        parse_openrouter(&text)?
    } else {
        parse_models_dev(&text)?
    };
    let snapshot = CatalogSnapshot::from_json(
        &serde_json::to_string(&snapshot).map_err(|error| error.to_string())?,
    )?;
    let cache = Cached {
        version: 1,
        source: source.into(),
        updated_at: now(),
        checksum: digest(&snapshot)?,
        snapshot,
    };
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut file, &cache).map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    file.persist(path(directory, source))
        .map_err(|error| error.to_string())?;
    install(catalog, source, cache.snapshot);
    Ok(())
}
pub(super) fn start(catalog: ModelCatalog, directory: PathBuf) {
    tokio::spawn(async move {
        let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
        else {
            return;
        };
        loop {
            // Each source updates independently; failed updates leave validated layers intact.
            for (source, url) in SOURCES {
                let _ = refresh(&catalog, &directory, &client, source, url).await;
            }
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_backend::Price;

    #[test]
    fn corrupt_cache_is_ignored_and_valid_cache_updates_existing_catalog_clones() {
        let directory = tempfile::tempdir().expect("directory");
        let catalog = ModelCatalog::default();
        let shared = catalog.clone();
        let snapshot = CatalogSnapshot::from_json(
            r#"{"providers":{"test":{"model":{
          "name":"Test","capabilities":{"context_window":16384},
          "price":{"input":1,"output":2,"cache_read":0,"cache_write":0}
        }}}}"#,
        )
        .expect("snapshot");
        let mut cache = Cached {
            version: 1,
            source: "models.dev".into(),
            updated_at: now(),
            checksum: digest(&snapshot).expect("digest"),
            snapshot,
        };
        let file = path(directory.path(), "models.dev");
        std::fs::write(&file, serde_json::to_vec(&cache).expect("json")).expect("save");
        load(&catalog, directory.path());
        let resolved = shared.resolve("test", "model");
        assert_eq!(resolved.capabilities.context_window, Some(16384));
        assert_eq!(resolved.price, Some(Price::per_million(1.0, 2.0, 0.0, 0.0)));
        assert_eq!(resolved.sources["context_window"], "cache:models.dev");
        cache
            .snapshot
            .providers
            .get_mut("test")
            .expect("provider")
            .get_mut("model")
            .expect("model")
            .capabilities
            .context_window = Some(99999);
        std::fs::write(&file, serde_json::to_vec(&cache).expect("json")).expect("save");
        load(&catalog, directory.path());
        assert_eq!(
            shared.resolve("test", "model").capabilities.context_window,
            Some(16384)
        );
        assert!(
            ModelCatalog::default()
                .resolve("test", "model")
                .price
                .is_none()
        );
    }

    #[test]
    fn source_precedence_is_independent_of_refresh_order() {
        let catalog = ModelCatalog::default();
        let router = parse_openrouter(
            r#"{"data":[{"id":"test","architecture":{"output_modalities":["text"]},
          "pricing":{"prompt":"0","completion":"0"}}]}"#,
        )
        .expect("router");
        install(&catalog, "openrouter", router);
        let dev = super::super::parse_models_dev(r#"{"openrouter":{"models":{"test":{
          "modalities":{"output":["text"]},"cost":{"text":{"input":{"price":1},"output":{"price":2}}}
        }}}}"#).expect("models.dev");
        install(&catalog, "models.dev", dev);
        assert_eq!(
            catalog.resolve("openrouter", "test").price,
            Some(Price::FREE)
        );
    }
}
