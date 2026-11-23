use anyhow::Result;
use solaris::tui::tips;
use solaris_agent::default_tool_names;
use solaris_provider::CredentialSource;

use super::Startup;

pub(super) fn print_config(startup: &Startup) -> Result<()> {
    let Startup {
        choice,
        config,
        registry,
        tools,
        resources,
        backend_options,
        auth,
        auth_path,
        buddy,
        buddy_path,
        recent,
        recent_path,
        settings,
        ..
    } = startup;
    let skills = &resources.skills;
    println!("backend: {}", choice.backend.label());
    println!(
        "capabilities: {}",
        serde_json::to_string(&choice.backend.capabilities())?
    );
    println!("model: {}", describe_model(&choice.model));
    println!("theme: {}", config.theme);
    println!("mode: {}", config.mode.label());
    println!(
        "tools: {}",
        registry
            .selected_names(default_tool_names(config.mode), tools)
            .join(", ")
    );
    if let Some(shell_path) = std::env::var_os("SOLARIS_SHELL_PATH")
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string_lossy().into_owned())
        .or_else(|| settings.shell_path.clone())
    {
        println!("shell path: {shell_path}");
    }
    if let Some(prefix) = &settings.shell_command_prefix {
        println!("shell command prefix: {prefix}");
    }
    println!(
        "skills: {}",
        if skills.is_empty() {
            "none".to_string()
        } else {
            skills
                .iter()
                .map(|skill| skill.name())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    let mut resolved = backend_options
        .configuration
        .resolve(choice.provider_id.as_deref().unwrap_or(""), &choice.model);
    if matches!(
        auth.credential("openai"),
        Some(solaris_provider::Credential::ChatGpt { .. })
    ) && choice.provider_id.as_deref() == Some("openai")
    {
        resolved = resolved.without_pricing();
    }
    println!(
        "model parameters: {}",
        serde_json::to_string_pretty(&resolved)?
    );
    match auth_path {
        Some(path) => println!("credentials: {}", path.display()),
        None => println!("credentials: (unsaved — no config directory)"),
    }
    println!(
        "connected: {}",
        if auth.is_empty() {
            "none".to_string()
        } else {
            auth.connected().join(", ")
        }
    );
    println!(
        "active provider: {}",
        auth.active_provider().unwrap_or("none")
    );
    println!(
        "endpoint: {}",
        choice.base_url.as_deref().unwrap_or("(not connected)")
    );
    println!("credential source: {}", describe_source(choice.source));
    let buddy_name = match &buddy.soul {
        Some(soul) => format!("{} the {}", soul.name, buddy.bones.species.as_str()),
        None => format!("{} (unnamed)", buddy.bones.species.as_str()),
    };
    println!(
        "buddy: {buddy_name} — {}, eyes {}, hat {}",
        buddy.bones.rarity.as_str(),
        buddy.bones.eye.glyph(),
        buddy.bones.hat.as_str()
    );
    match buddy_path {
        Some(path) => println!("companion: {}", path.display()),
        None => println!("companion: (unsaved — no config directory)"),
    }
    match recent_path {
        Some(path) => println!("recent activity: {}", path.display()),
        None => println!("recent activity: (unsaved — no config directory)"),
    }
    println!(
        "recent prompts: {}",
        if recent.is_empty() {
            "none".to_string()
        } else {
            recent
                .entries()
                .iter()
                .map(|entry| entry.label.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        }
    );
    println!("tip: {}", tips::select(recent.len()).content);
    Ok(())
}

/// The model a run will ask for, or an explanation when none was named and no
/// provider was around to supply one.
fn describe_model(model: &str) -> &str {
    if model.is_empty() {
        "(none yet — run /connect)"
    } else {
        model
    }
}

/// Where the credential a backend will use came from, for `--print-config`.
fn describe_source(source: CredentialSource) -> String {
    match source {
        CredentialSource::Stored => "auth.json".to_string(),
        CredentialSource::Env(name) => format!("environment ({name})"),
        CredentialSource::None => "none needed".to_string(),
        CredentialSource::Missing => "none — not connected".to_string(),
        CredentialSource::NoModel => "present — no model named for it yet".to_string(),
    }
}
