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
