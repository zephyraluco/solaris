//! Approval rules are tool policy, not an operating-system sandbox.
use crate::{Approval, Approver};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solaris_backend::ToolCall;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyProfile {
    #[default]
    Trusted,
    Ask,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionEffect {
    Allow,
    Ask,
    Deny,
}

/// Last matching rule wins. Resource patterns use '*' and normalized absolute paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: String,
    pub resource: String,
    pub effect: PermissionEffect,
}

pub struct PolicyApprover {
    pub profile: PolicyProfile,
    pub read_only_tools: std::collections::BTreeSet<String>,
    pub cwd: PathBuf,
    pub rules: Vec<PermissionRule>,
    pub fallback: Arc<dyn Approver>,
}

fn matches(pattern: &str, text: &str) -> bool {
    let (mut p, mut t, mut star, mut retry) = (0, 0, None, 0);
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    while t < text.len() {
        if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if let Some(at) = star {
            retry += 1;
            t = retry;
            p = at + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn path_rule_matches(tool: &str, pattern: &str, resource: &str) -> bool {
    if tool == "bash" {
        return matches(pattern, resource);
    }
    let environment = solaris_tools::environment::ExecutionEnvironment;
    matches(
        &environment.comparison_path(pattern),
        &environment.comparison_path(resource),
    )
}

/// Canonicalize existing ancestors so symlink aliases cannot bypass a path rule.
fn normalized(path: &Path) -> Result<PathBuf, std::io::Error> {
    solaris_tools::environment::ExecutionEnvironment.canonical_path(path)
}

#[async_trait]
impl Approver for PolicyApprover {
    async fn approve(&self, call: &ToolCall) -> Approval {
        let read_only = call.name == "tool_search"
            || self.read_only_tools.contains(&call.name)
            || (call.name == "subagent" && call.input["agent"] == "explore");
        // This upper bound cannot be relaxed by a rule inherited by a child.
        if self.profile == PolicyProfile::ReadOnly && !read_only {
            return Approval::Denied;
        }
        if self.rules.is_empty() {
            return if self.profile == PolicyProfile::Trusted
                || (self.profile == PolicyProfile::Ask && !read_only)
            {
                self.fallback.approve(call).await
            } else {
                Approval::Approved
            };
        }
        let resource = if call.name == "bash" {
            // Shells consume command text, not path. Extra ignored arguments
            // must never change which command permission rule applies.
            let Some(command) = call.input["command"].as_str() else {
                return Approval::Denied;
            };
            command.to_owned()
        } else {
            // Match the tools' optional-path parser, including blank/null.
            let path = if matches!(call.name.as_str(), "grep" | "find" | "ls") {
                match solaris_tools::optional_string_arg(&call.input, "path") {
                    Ok(path) => Some(path.unwrap_or_else(|| ".".into())),
                    Err(_) => return Approval::Denied,
                }
            } else {
                call.input["path"].as_str().map(str::to_owned)
            };
            if let Some(path) = path {
                let cwd = self.cwd.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let path = solaris_tools::environment::resolve_path(&cwd, &path);
                    normalized(&path)
                })
                .await;
                match result {
                    Ok(Ok(path)) => path.to_string_lossy().into_owned(),
                    _ => return Approval::Denied,
                }
            } else {
                call.input["command"].as_str().unwrap_or("*").to_string()
            }
        };
        let effect = self
            .rules
            .iter()
            .rev()
            .find(|rule| {
                matches(&rule.tool, &call.name)
                    && path_rule_matches(&call.name, &rule.resource, &resource)
            })
            .map(|r| r.effect)
            .unwrap_or(
                if self.profile == PolicyProfile::Trusted
                    || (self.profile == PolicyProfile::Ask && !read_only)
                {
                    PermissionEffect::Ask
                } else {
                    PermissionEffect::Allow
                },
            );
        match effect {
            PermissionEffect::Allow => Approval::Approved,
            PermissionEffect::Deny => Approval::Denied,
            PermissionEffect::Ask => self.fallback.approve(call).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlwaysApprove;
    #[tokio::test]
    async fn a_rule_cannot_raise_the_read_only_ceiling() {
        let policy = PolicyApprover {
            profile: PolicyProfile::ReadOnly,
            read_only_tools: ["read".into()].into_iter().collect(),
            cwd: std::env::current_dir().expect("cwd"),
            rules: vec![PermissionRule {
                tool: "*".into(),
                resource: "*".into(),
                effect: PermissionEffect::Allow,
            }],
            fallback: Arc::new(AlwaysApprove),
        };
        assert_eq!(
            policy
                .approve(&ToolCall::new(
                    "call",
                    "bash",
                    serde_json::json!({"command":"echo test"})
                ))
                .await,
            Approval::Denied
        );
        assert_eq!(
            policy
                .approve(&ToolCall::new(
                    "call",
                    "read",
                    serde_json::json!({"path":"Cargo.toml"})
                ))
                .await,
            Approval::Approved
        );
    }
    #[test]
    fn wildcard_rules_match_the_complete_resource() {
        assert!(matches("*/src/*.rs", "/project/src/main.rs"));
        assert!(!matches("read", "read_secret"));
    }

    #[tokio::test]
    async fn reads_without_matching_ask_rules_remain_automatic() {
        for profile in [PolicyProfile::Ask, PolicyProfile::ReadOnly] {
            for rules in [
                vec![],
                vec![PermissionRule {
                    tool: "write".into(),
                    resource: "*".into(),
                    effect: PermissionEffect::Ask,
                }],
            ] {
                let policy = PolicyApprover {
                    profile,
                    rules,
                    read_only_tools: ["read".into()].into_iter().collect(),
                    cwd: std::env::current_dir().expect("cwd"),
                    fallback: Arc::new(crate::DenyAll),
                };
                assert_eq!(
                    policy
                        .approve(&ToolCall::new(
                            "read",
                            "read",
                            serde_json::json!({"path":"Cargo.toml"})
                        ))
                        .await,
                    Approval::Approved
                );
            }
        }
    }

    #[tokio::test]
    async fn trusted_custom_approvers_can_still_refuse_reads() {
        let policy = PolicyApprover {
            profile: PolicyProfile::Trusted,
            rules: Vec::new(),
            read_only_tools: ["read".into()].into_iter().collect(),
            cwd: std::env::current_dir().expect("cwd"),
            fallback: Arc::new(crate::DenyAll),
        };
        assert_eq!(
            policy
                .approve(&ToolCall::new(
                    "read",
                    "read",
                    serde_json::json!({"path":"Cargo.toml"})
                ))
                .await,
            Approval::Denied
        );
    }

    #[tokio::test]
    async fn omitted_builtin_paths_cannot_bypass_directory_deny_rules() {
        let directory = tempfile::tempdir().expect("directory");
        let cwd = directory.path().canonicalize().expect("cwd");
        for profile in [
            PolicyProfile::Trusted,
            PolicyProfile::Ask,
            PolicyProfile::ReadOnly,
        ] {
            for name in ["grep", "find", "ls"] {
                let policy = PolicyApprover {
                    profile,
                    cwd: cwd.clone(),
                    read_only_tools: [name.to_owned()].into_iter().collect(),
                    rules: vec![PermissionRule {
                        tool: name.into(),
                        resource: cwd.to_string_lossy().into_owned(),
                        effect: PermissionEffect::Deny,
                    }],
                    fallback: Arc::new(AlwaysApprove),
                };
                for input in [
                    serde_json::json!({"pattern":"x"}),
                    serde_json::json!({"pattern":"x","path":"."}),
                    serde_json::json!({"pattern":"x","path":null}),
                    serde_json::json!({"pattern":"x","path":""}),
                    serde_json::json!({"pattern":"x","path":"  \t"}),
                ] {
                    assert_eq!(
                        policy.approve(&ToolCall::new("call", name, input)).await,
                        Approval::Denied,
                        "{name} / {profile:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn omitted_builtin_paths_use_the_same_ask_hook_as_explicit_dot() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().expect("directory");
        let cwd = directory.path().canonicalize().expect("cwd");
        for profile in [PolicyProfile::Ask, PolicyProfile::ReadOnly] {
            for name in ["grep", "find", "ls"] {
                let called = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&called);
                let policy = PolicyApprover {
                    profile,
                    cwd: cwd.clone(),
                    read_only_tools: [name.to_owned()].into_iter().collect(),
                    rules: vec![PermissionRule {
                        tool: name.into(),
                        resource: cwd.to_string_lossy().into_owned(),
                        effect: PermissionEffect::Ask,
                    }],
                    fallback: Arc::new(crate::FnApprover::new(move |_: &ToolCall| {
                        observed.fetch_add(1, Ordering::SeqCst);
                        Approval::Denied
                    })),
                };
                for input in [
                    serde_json::json!({"pattern":"x"}),
                    serde_json::json!({"pattern":"x","path":"."}),
                    serde_json::json!({"pattern":"x","path":null}),
                    serde_json::json!({"pattern":"x","path":""}),
                    serde_json::json!({"pattern":"x","path":"  \t"}),
                ] {
                    assert_eq!(
                        policy.approve(&ToolCall::new("call", name, input)).await,
                        Approval::Denied
                    );
                }
                assert_eq!(
                    called.load(Ordering::SeqCst),
                    5,
                    "{name} must ask for all default-path forms"
                );
            }
        }
    }

    #[tokio::test]
    async fn explicit_search_paths_are_not_replaced_by_the_default_directory() {
        let directory = tempfile::tempdir().expect("directory");
        let cwd = directory.path().canonicalize().expect("cwd");
        std::fs::create_dir(cwd.join("restricted")).expect("restricted directory");
        std::fs::create_dir(cwd.join("allowed")).expect("allowed directory");
        for name in ["grep", "find", "ls"] {
            let policy = PolicyApprover {
                profile: PolicyProfile::ReadOnly,
                cwd: cwd.clone(),
                read_only_tools: [name.to_owned()].into_iter().collect(),
                rules: vec![PermissionRule {
                    tool: name.into(),
                    resource: cwd.join("restricted").to_string_lossy().into_owned(),
                    effect: PermissionEffect::Deny,
                }],
                fallback: Arc::new(AlwaysApprove),
            };
            for (path, expected) in [
                ("restricted", Approval::Denied),
                ("allowed", Approval::Approved),
                (".", Approval::Approved),
            ] {
                assert_eq!(
                    policy
                        .approve(&ToolCall::new(
                            "call",
                            name,
                            serde_json::json!({"pattern":"x","path":path})
                        ))
                        .await,
                    expected
                );
            }
        }
    }

    #[tokio::test]
    async fn shell_command_rules_cannot_be_bypassed_by_ignored_path_fields() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        {
            let name = "bash";
            for effect in [PermissionEffect::Deny, PermissionEffect::Ask] {
                let called = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&called);
                let policy = PolicyApprover {
                    profile: PolicyProfile::Trusted,
                    cwd: std::env::current_dir().expect("cwd"),
                    read_only_tools: Default::default(),
                    rules: vec![PermissionRule {
                        tool: name.into(),
                        resource: "blocked command".into(),
                        effect,
                    }],
                    fallback: Arc::new(crate::FnApprover::new(move |_: &ToolCall| {
                        observed.fetch_add(1, Ordering::SeqCst);
                        if effect == PermissionEffect::Ask {
                            Approval::Denied
                        } else {
                            Approval::Approved
                        }
                    })),
                };
                for path in [
                    None,
                    Some(serde_json::json!(".")),
                    Some(serde_json::json!("")),
                    Some(serde_json::json!(42)),
                ] {
                    let mut input = serde_json::json!({"command":"blocked command"});
                    if let Some(path) = path {
                        input["path"] = path;
                    }
                    assert_eq!(
                        policy.approve(&ToolCall::new("call", name, input)).await,
                        Approval::Denied
                    );
                }
                assert_eq!(
                    called.load(Ordering::SeqCst),
                    if effect == PermissionEffect::Ask {
                        4
                    } else {
                        0
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn shell_command_allow_rules_are_not_overridden_by_ignored_paths() {
        let cwd = std::env::current_dir()
            .expect("cwd")
            .canonicalize()
            .expect("canonical cwd");
        {
            let name = "bash";
            let policy = PolicyApprover {
                profile: PolicyProfile::Trusted,
                cwd: cwd.clone(),
                read_only_tools: Default::default(),
                rules: vec![
                    PermissionRule {
                        tool: name.into(),
                        resource: "safe command".into(),
                        effect: PermissionEffect::Allow,
                    },
                    PermissionRule {
                        tool: name.into(),
                        resource: cwd.to_string_lossy().into_owned(),
                        effect: PermissionEffect::Deny,
                    },
                ],
                fallback: Arc::new(crate::DenyAll),
            };
            assert_eq!(
                policy
                    .approve(&ToolCall::new(
                        "call",
                        name,
                        serde_json::json!({"command":"safe command","path":"."})
                    ))
                    .await,
                Approval::Approved
            );
        }
    }
}
