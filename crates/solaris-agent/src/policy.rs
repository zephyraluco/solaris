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

/// Canonicalize existing ancestors so symlink aliases cannot bypass a path rule.
fn normalized(path: &Path) -> Result<PathBuf, std::io::Error> {
    let clean = path.to_path_buf();
    let mut parent = clean.as_path();
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(parent) {
            Ok(mut base) => {
                for part in suffix.iter().rev() {
                    base.push(part);
                }
                return Ok(base);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(parent.file_name().ok_or(error)?);
                parent = parent
                    .parent()
                    .ok_or_else(|| std::io::Error::other("path has no existing ancestor"))?;
            }
            Err(error) => return Err(error),
        }
    }
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
        let resource = if let Some(path) = call.input["path"].as_str() {
            let path = Path::new(path);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.cwd.join(path)
            };
            match normalized(&path) {
                Ok(path) => path.to_string_lossy().into_owned(),
                Err(_) => return Approval::Denied,
            }
        } else {
            call.input["command"].as_str().unwrap_or("*").to_string()
        };
        let effect = self
            .rules
            .iter()
            .rev()
            .find(|rule| matches(&rule.tool, &call.name) && matches(&rule.resource, &resource))
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
}
