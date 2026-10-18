//! A skipped test must state its prerequisite beside the attribute.
use anyhow::{bail, Result};

fn missing_reason(source: &str) -> bool {
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        if !line.trim_start().starts_with("#[") {
            continue;
        }
        let mut attributes = line.to_string();
        while !attributes.contains(']') {
            let Some(line) = lines.next() else { break };
            attributes.push_str(line);
        }
        for attr in attributes.split("#[").skip(1).filter_map(|s| s.split_once(']').map(|(a, _)| a.trim())) {
            let Some(rest) = attr.strip_prefix("ignore") else { continue };
            if !(rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('=')) {
                continue;
            }
            let reason =
                rest.trim().strip_prefix('=').map(str::trim).and_then(|s| serde_json::from_str::<String>(s).ok());
            if reason.is_none_or(|s| s.trim().is_empty()) {
                return true;
            }
        }
    }
    false
}

pub fn check() -> Result<()> {
    fn walk(path: &std::path::Path) -> Result<()> {
        for entry in std::fs::read_dir(path)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path)?;
            } else if path.extension().is_some_and(|e| e == "rs") && missing_reason(&std::fs::read_to_string(&path)?) {
                bail!("{}: ignored tests need a nonempty reason in the ignore attribute", path.display());
            }
        }
        Ok(())
    }
    for dir in ["src", "tests", "xtask/src", "examples"] {
        let path = super::root().join(dir);
        if path.exists() {
            walk(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::missing_reason;
    #[test]
    fn reasons_are_required_for_bare_empty_and_multiline_attributes() {
        assert!(missing_reason("#[ignore]\nfn skipped() {}"));
        assert!(missing_reason("#[ignore = \" \"]"));
        assert!(missing_reason("#[\nignore\n]"));
        assert!(!missing_reason("#[ignore = \"requires a live model\"]"));
        assert!(!missing_reason("#[ignore =\n \"requires credentials\"\n]"));
        assert!(!missing_reason("//! #[ignore] is documentation\nlet s = \"#[ignore]\";"));
    }
}
