use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalKind {
    Missing,
    Refused,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Build { drv: String },
    Eval { attr: String, kind: EvalKind },
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
static CANNOT_BUILD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Cannot build '(/nix/store/[^']+\.drv)'\.\s*Reason: ([^\n]*)").unwrap()
});
static BUILDER_FAILED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"builder for '(/nix/store/[^']+\.drv)' failed").unwrap());
static MISSING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"attribute '([^']+)' missing").unwrap());
static REFUSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Refusing to evaluate package '([^']+)' in (\S+?)(?::\d+)? because").unwrap()
});
static BY_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/pkgs/by-name/[^/]+/([^/]+)/package\.nix$").unwrap());
static REMOVED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"'([^']+)' has been (?:removed|renamed|replaced)").unwrap()
});
static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?)-[0-9]").unwrap());

pub fn strip_ansi(s: &str) -> String {
    ANSI.replace_all(s, "").into_owned()
}

pub fn strip_version(name: &str) -> &str {
    VERSION
        .captures(name)
        .and_then(|c| c.get(1))
        .map_or(name, |m| m.as_str())
}

/// The text of an error-level message from a `--log-format internal-json` stderr line.
pub fn error_message(line: &str) -> Option<String> {
    let json: Value = serde_json::from_str(line.strip_prefix("@nix ")?).ok()?;
    if json["action"] != "msg" || json["level"] != 0 {
        return None;
    }
    json["msg"].as_str().map(strip_ansi)
}

pub fn parse(messages: &[String]) -> Vec<Failure> {
    let mut out = Vec::new();
    let mut push = |f: Failure| {
        if !out.contains(&f) {
            out.push(f);
        }
    };

    for msg in messages {
        let msg = strip_ansi(msg);

        let mut found_build = false;
        for c in CANNOT_BUILD.captures_iter(&msg) {
            found_build = true;
            if !c[2].contains("dependency failed") && !c[2].contains("dependencies failed") {
                push(Failure::Build { drv: c[1].to_string() });
            }
        }
        for c in BUILDER_FAILED.captures_iter(&msg) {
            found_build = true;
            push(Failure::Build { drv: c[1].to_string() });
        }
        if found_build {
            continue;
        }

        if let Some(c) = REFUSED.captures(&msg) {
            let attr = BY_NAME
                .captures(&c[2])
                .map(|b| b[1].to_string())
                .unwrap_or_else(|| strip_version(&c[1]).to_string());
            push(Failure::Eval { attr, kind: EvalKind::Refused });
        } else if let Some(c) = REMOVED.captures(&msg) {
            push(Failure::Eval { attr: c[1].to_string(), kind: EvalKind::Removed });
        } else if let Some(c) = MISSING.captures(&msg) {
            push(Failure::Eval { attr: c[1].to_string(), kind: EvalKind::Missing });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_messages(text: &str) -> Vec<String> {
        text.lines().filter_map(error_message).collect()
    }

    #[test]
    fn new_build_failure_format_skips_dependents() {
        let msgs = fixture_messages(include_str!("../tests/fixtures/build-failure.jsonl"));
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            parse(&msgs),
            vec![Failure::Build {
                drv: "/nix/store/w7w5jmhzl1x5489mv05b6ai9994bvzyx-libfoo-1.2.drv".into()
            }]
        );
    }

    #[test]
    fn old_build_failure_format() {
        let msgs = vec![
            "error: builder for '/nix/store/abc-foo-1.0.drv' failed with exit code 2".to_string(),
            "error: 1 dependencies of derivation '/nix/store/def-bar-1.0.drv' failed to build".to_string(),
        ];
        assert_eq!(
            parse(&msgs),
            vec![Failure::Build { drv: "/nix/store/abc-foo-1.0.drv".into() }]
        );
    }

    #[test]
    fn eval_missing_attribute() {
        let msgs = fixture_messages(include_str!("../tests/fixtures/eval-missing.jsonl"));
        assert_eq!(
            parse(&msgs),
            vec![Failure::Eval { attr: "nodejs_18".into(), kind: EvalKind::Missing }]
        );
    }

    #[test]
    fn eval_refused_prefers_by_name_directory() {
        let msgs = vec![
            "error: Refusing to evaluate package 'hello-2.12.3' in /nix/store/x-source/pkgs/by-name/he/hello/package.nix:16 because it has problems:".to_string(),
            "error: Refusing to evaluate package 'cuda-merged-12.9' in /nix/store/x-source/pkgs/development/cuda-modules/packages/cudatoolkit.nix:81 because it has an unfree license".to_string(),
        ];
        assert_eq!(
            parse(&msgs),
            vec![
                Failure::Eval { attr: "hello".into(), kind: EvalKind::Refused },
                Failure::Eval { attr: "cuda-merged".into(), kind: EvalKind::Refused },
            ]
        );
    }

    #[test]
    fn eval_removed_alias() {
        let msgs = vec![
            "error: '_0verkill' has been removed due to lack of maintenance".to_string(),
            "error: '_1password' has been renamed to/replaced by '_1password-cli'".to_string(),
        ];
        assert_eq!(
            parse(&msgs),
            vec![
                Failure::Eval { attr: "_0verkill".into(), kind: EvalKind::Removed },
                Failure::Eval { attr: "_1password".into(), kind: EvalKind::Removed },
            ]
        );
    }

    #[test]
    fn ignores_non_error_lines() {
        assert_eq!(error_message(r#"@nix {"action":"msg","level":3,"msg":"these 2 derivations will be built:"}"#), None);
        assert_eq!(error_message("plain text"), None);
        assert!(parse(&["error: something unrelated".to_string()]).is_empty());
    }

    #[test]
    fn version_stripping() {
        assert_eq!(strip_version("obs-studio-31.0.1"), "obs-studio");
        assert_eq!(strip_version("hello"), "hello");
        assert_eq!(strip_version("python3.13-requests-2.32"), "python3.13-requests");
    }
}
