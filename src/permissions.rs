//! Maps script-declared Deno capabilities onto CLI flags.

use std::path::Path;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Allow and deny lists for a single Deno capability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CapabilityPermission {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

impl CapabilityPermission {
    pub fn allow_only(allow: Vec<String>) -> Self {
        Self {
            allow,
            deny: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
}

impl<'de> Deserialize<'de> for CapabilityPermission {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CapabilityVisitor;

        impl<'de> Visitor<'de> for CapabilityVisitor {
            type Value = CapabilityPermission;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string array or { allow?: string[], deny?: string[] }")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut allow = Vec::new();
                while let Some(value) = seq.next_element::<String>()? {
                    allow.push(value);
                }
                Ok(CapabilityPermission::allow_only(allow))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut allow = Vec::new();
                let mut deny = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "allow" => allow = map.next_value()?,
                        "deny" => deny = map.next_value()?,
                        _ => {
                            let _: de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(CapabilityPermission { allow, deny })
            }
        }

        deserializer.deserialize_any(CapabilityVisitor)
    }
}

/// Capability lists declared by a script's `permissions` field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptPermissions {
    pub read: CapabilityPermission,
    pub write: CapabilityPermission,
    pub net: CapabilityPermission,
    pub env: CapabilityPermission,
    pub run: CapabilityPermission,
    pub sys: CapabilityPermission,
}

/// Convert a script's `permissions` object into Deno CLI flags.
///
/// Each non-empty `allow` list becomes `--allow-<capability>`; each non-empty
/// `deny` list becomes `--deny-<capability>`. Entries equal to `*` request the
/// unrestricted form of that flag. Other entries are joined with commas after
/// expanding `${workspace}` to `workspace`.
pub fn to_deno_flags(permissions: &ScriptPermissions, workspace: &Path) -> Vec<String> {
    let mut flags = Vec::new();
    for (capability, permission) in [
        ("read", &permissions.read),
        ("write", &permissions.write),
        ("net", &permissions.net),
        ("env", &permissions.env),
        ("run", &permissions.run),
        ("sys", &permissions.sys),
    ] {
        push_flag(&mut flags, "allow", capability, &permission.allow, workspace);
        push_flag(&mut flags, "deny", capability, &permission.deny, workspace);
    }
    flags
}

fn push_flag(
    flags: &mut Vec<String>,
    mode: &str,
    capability: &str,
    entries: &[String],
    workspace: &Path,
) {
    if entries.is_empty() {
        return;
    }
    if entries.iter().any(|entry| entry.trim() == "*") {
        flags.push(format!("--{mode}-{capability}"));
        return;
    }
    let expanded: Vec<String> = entries
        .iter()
        .map(|entry| expand_workspace(entry, workspace))
        .filter(|entry| !entry.is_empty())
        .collect();
    if expanded.is_empty() {
        return;
    }
    flags.push(format!("--{mode}-{capability}={}", expanded.join(",")));
}

fn expand_workspace(value: &str, workspace: &Path) -> String {
    value.replace("${workspace}", &workspace.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn maps_scoped_allow_and_deny_permissions() {
        let workspace = PathBuf::from("/proj");
        let permissions = ScriptPermissions {
            read: CapabilityPermission {
                allow: vec!["~/*".into()],
                deny: vec!["~/.ssh/*".into()],
            },
            write: CapabilityPermission {
                allow: vec!["${workspace}/*".into()],
                deny: vec!["${workspace}/.secrets/*".into()],
            },
            net: CapabilityPermission::allow_only(vec!["api.github.com".into()]),
            env: CapabilityPermission::allow_only(vec!["GITHUB_TOKEN".into()]),
            run: CapabilityPermission::allow_only(vec!["git".into()]),
            sys: CapabilityPermission::allow_only(vec!["*".into()]),
        };
        assert_eq!(
            to_deno_flags(&permissions, &workspace),
            vec![
                "--allow-read=~/*",
                "--deny-read=~/.ssh/*",
                "--allow-write=/proj/*",
                "--deny-write=/proj/.secrets/*",
                "--allow-net=api.github.com",
                "--allow-env=GITHUB_TOKEN",
                "--allow-run=git",
                "--allow-sys",
            ]
        );
    }

    #[test]
    fn deserializes_shorthand_allow_lists() {
        let permissions: ScriptPermissions = serde_json::from_value(json!({
            "read": ["${workspace}"],
            "net": ["api.github.com"],
        }))
        .unwrap();
        assert_eq!(
            permissions.read,
            CapabilityPermission::allow_only(vec!["${workspace}".into()])
        );
        assert!(permissions.write.is_empty());
        assert_eq!(
            permissions.net,
            CapabilityPermission::allow_only(vec!["api.github.com".into()])
        );
    }

    #[test]
    fn deserializes_allow_deny_objects() {
        let permissions: ScriptPermissions = serde_json::from_value(json!({
            "read": {
                "allow": ["~/*"],
                "deny": ["~/.ssh/*"],
            },
        }))
        .unwrap();
        assert_eq!(
            permissions.read,
            CapabilityPermission {
                allow: vec!["~/*".into()],
                deny: vec!["~/.ssh/*".into()],
            }
        );
    }

    #[test]
    fn skips_empty_capability_lists() {
        let workspace = PathBuf::from("/proj");
        assert!(to_deno_flags(&ScriptPermissions::default(), &workspace).is_empty());
    }
}
