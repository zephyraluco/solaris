use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use solaris::tui::buddy::{Companion, load_soul};
use solaris::{Config, Mode, Preferences, RecentActivity};
use solaris_agent::{ResourcePaths, ResourceSnapshot, ToolSelection};
use solaris_provider::{AuthStore, BackendChoice, BackendOptions, choose_backend, provider_spec};

use solaris::application::{
    resources::read_mcp_config,
    startup::{config_dir, home_dir, load_auth, load_recent, load_settings, user_id},
};

use super::Args;

pub(super) struct Startup {
    pub(super) config: Config,
    pub(super) auth: AuthStore,
    pub(super) auth_path: Option<PathBuf>,
    pub(super) buddy: Companion,
    pub(super) buddy_path: Option<PathBuf>,
    pub(super) recent: RecentActivity,
    pub(super) recent_path: Option<PathBuf>,
    pub(super) settings: Preferences,
    pub(super) settings_path: Option<PathBuf>,
    pub(super) user: String,
    pub(super) resource_paths: ResourcePaths,
    pub(super) resources: ResourceSnapshot,
    pub(super) tools: ToolSelection,
    pub(super) registry: solaris_tools::ToolRegistry,
    pub(super) backend_options: BackendOptions,
    pub(super) choice: BackendChoice,
    pub(super) state_dir: Option<PathBuf>,
}

impl Startup {
    pub(super) fn load(args: &Args) -> Result<Self> {
        let mut config = Config {
            theme: args
                .theme
                .clone()
                .unwrap_or_else(|| Config::default().theme),
            mode: if args.plan { Mode::Plan } else { Mode::Build },
            ..Config::default()
        };
        if let Some(model) = &args.model {
            config.model = model.clone();
        }

        let tools = match ToolSelection::parse(args.tools.as_deref(), args.exclude_tools.as_deref())
        {
            Ok(selection) => selection,
            Err(message) => anyhow::bail!("{message}"),
        };
        // A name that matches nothing is almost always a typo, and silently
        // declaring nothing would look like the tool simply refused to run.
        let registry = solaris_agent::agent_tool_registry();

        let state_dir = config_dir();
        let auth_path = state_dir.as_ref().map(|dir| dir.join("auth.json"));
        let buddy_path = state_dir.as_ref().map(|dir| dir.join("companion.json"));
        let recent_path = state_dir.as_ref().map(|dir| dir.join("recent.json"));
        let settings_path = state_dir.as_ref().map(|dir| dir.join("settings.json"));

        let mut auth = load_auth(auth_path.as_deref())?;
        let user = user_id();
        let buddy = Companion::new(&user, load_soul(buddy_path.as_deref()));
        let recent = load_recent(recent_path.as_deref());
        let settings = load_settings(settings_path.as_deref());

        let resource_paths = ResourcePaths {
            cwd: std::env::current_dir()?,
            config_dir: state_dir.clone(),
            home_dir: home_dir(),
            extra_skills: args.skill.clone(),
        };
        let resources = resource_paths.load()?;
        for warning in resources
            .skills
            .warnings()
            .iter()
            .chain(&resources.warnings)
        {
            eprintln!("solaris: {} — {}", warning.path.display(), warning.message);
        }

        // What the last session left, unless this run overrides it: a flag always
        // wins, so `--plan` in a script keeps meaning plan. The model follows the
        // provider it was chosen for, and is kept in `auth.json` beside it.
        if args.theme.is_none()
            && let Some(theme) = &settings.theme
        {
            config.theme = theme.clone();
        }
        if !args.plan
            && let Some(mode) = settings.mode
        {
            config.mode = mode;
        }

        let mut backend_options = BackendOptions::default();
        if let Some(directory) = &state_dir {
            let path = directory.join("models.json");
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    backend_options.configuration = Arc::new(
                        solaris_provider::ProviderConfig::from_json(&text)
                            .map_err(anyhow::Error::msg)?,
                    )
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let cache_dir = directory.join("model-cache");
            backend_options.model_cache_dir = Some(cache_dir);
        }
        // Custom endpoints are validated after their configuration has been loaded.
        if let Some(id) = &args.provider {
            if provider_spec(id).is_none() && backend_options.configuration.endpoint(id).is_none() {
                anyhow::bail!("unknown provider `{id}` — built-ins: {}", provider_ids());
            }
            auth.activate(id.clone());
        }
        if args.model.is_none()
            && let Some(model) = auth.active_model()
        {
            config.model = model.to_owned();
        }
        // Resolved here for reporting, and again by the app whenever the
        // credentials or the model change.
        let choice = choose_backend(&auth, &config.model, backend_options.clone());

        if args.provider.is_some() && choice.provider_id.is_none() {
            eprintln!(
                "solaris: {} has no usable credentials — run /connect, or set its key in the environment",
                args.provider.as_deref().unwrap_or_default()
            );
        }

        Ok(Self {
            config,
            auth,
            auth_path,
            buddy,
            buddy_path,
            recent,
            recent_path,
            settings,
            settings_path,
            user,
            resource_paths,
            resources,
            tools,
            registry,
            backend_options,
            choice,
            state_dir,
        })
    }

    pub(super) fn execution(
        &self,
        args: &Args,
    ) -> Result<(solaris_agent::ExecutionSettings, bool)> {
        let state_dir = self.state_dir.as_deref();
        let rules_path = state_dir.map(|dir| dir.join("permissions.json"));
        let rules = match rules_path.as_deref().map(std::fs::read_to_string) {
            Some(Ok(text)) => serde_json::from_str(&text)?,
            Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error.into());
            }
            _ => Vec::new(),
        };
        let execution = solaris_agent::ExecutionSettings {
            profile: match args.policy.as_str() {
                "ask" => solaris_agent::PolicyProfile::Ask,
                "read-only" => solaris_agent::PolicyProfile::ReadOnly,
                _ => solaris_agent::PolicyProfile::Trusted,
            },
            rules,
            steps: args.steps,
            subagent_depth: args.subagent_depth,
        };
        let ask_approval =
            solaris::application::execution::requires_approval(&execution, args.confirm_tools);
        Ok((execution, ask_approval))
    }

    pub(super) fn mcp_config(&self, args: &Args) -> Result<solaris_mcp::McpConfig> {
        let default_path = self.state_dir.as_ref().map(|dir| dir.join("mcp.json"));
        read_mcp_config(
            args.mcp_config.as_deref().or(default_path.as_deref()),
            args.mcp_config.is_some(),
        )
    }
}

fn provider_ids() -> String {
    solaris_provider::PROVIDERS
        .iter()
        .map(|spec| spec.id)
        .collect::<Vec<_>>()
        .join(", ")
}
