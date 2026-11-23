//! Common execution settings and the approval behavior selected by each adapter.

use solaris_agent::{ExecutionSettings, PermissionEffect, PolicyProfile};

pub fn requires_approval(settings: &ExecutionSettings, confirm_tools: bool) -> bool {
    confirm_tools
        || settings.profile == PolicyProfile::Ask
        || settings
            .rules
            .iter()
            .any(|rule| rule.effect == PermissionEffect::Ask)
}

pub fn with_approval(
    mut settings: ExecutionSettings,
    interactive_approval: bool,
) -> ExecutionSettings {
    if interactive_approval && settings.profile == PolicyProfile::Trusted {
        settings.profile = PolicyProfile::Ask;
    }
    settings
}
