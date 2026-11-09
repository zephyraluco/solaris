//! Generate a compact snapshot from downloaded upstream JSON with runtime adapters.
//! cargo run -p solaris-provider --example generate_catalog -- MODELS_DEV OUTPUT
use solaris_provider::parse_models_dev;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("expected MODELS_DEV OUTPUT paths".into());
    }
    let mut snapshot = parse_models_dev(&std::fs::read_to_string(&args[0])?)?;
    snapshot.retain_snapshot_providers();
    let text = serde_json::to_string(&snapshot)?;
    solaris_provider::CatalogSnapshot::from_json(&text)?;
    std::fs::write(&args[1], format!("{text}\n"))?;
    Ok(())
}
