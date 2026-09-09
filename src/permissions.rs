//! Maps script-declared permission names onto Deno CLI flags.

/// Convert a script's `permissions` export into Deno CLI flags.
///
/// Accepted forms:
/// - shorthand: `"net"`, `"read"`, `"env"`
/// - already-flagged: `"--allow-net"`, `"--allow-read=/tmp"`
/// - `"all"` for `--allow-all`
pub fn to_deno_flags(permissions: &[String]) -> Vec<String> {
    let mut flags = Vec::new();
    for permission in permissions {
        let trimmed = permission.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "all" {
            flags.push("--allow-all".to_string());
            continue;
        }
        if trimmed.starts_with("--") {
            flags.push(trimmed.to_string());
            continue;
        }
        flags.push(format!("--allow-{trimmed}"));
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_shorthand_and_passthrough() {
        let flags = to_deno_flags(&[
            "net".into(),
            "--allow-read=/tmp".into(),
            "all".into(),
            "".into(),
        ]);
        assert_eq!(
            flags,
            vec!["--allow-net", "--allow-read=/tmp", "--allow-all"]
        );
    }
}
