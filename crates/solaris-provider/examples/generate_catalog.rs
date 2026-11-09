//! Generate a compact snapshot from downloaded upstream JSON with runtime adapters.
//! cargo run -p solaris-provider --example generate_catalog -- MODELS_DEV OPENROUTER OUTPUT
use solaris_provider::{parse_models_dev, parse_openrouter};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err("expected MODELS_DEV OPENROUTER OUTPUT paths".into());
    }
    let mut snapshot = parse_models_dev(&std::fs::read_to_string(&args[0])?)?;
    let router = parse_openrouter(&std::fs::read_to_string(&args[1])?)?;
    snapshot.providers.extend(router.providers);
    let text = serde_json::to_string(&snapshot)?;
    solaris_provider::CatalogSnapshot::from_json(&text)?;
    std::fs::write(&args[2], format!("{text}\n"))?;
    Ok(())
}
