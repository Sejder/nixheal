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
static TRACE_OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"while evaluating the option `(?:systemd\.(?:user\.)?)?services\."?([A-Za-z0-9_+-]+)"?"#).unwrap()
});
static TRACE_DRV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"while evaluating derivation '([^']+)'").unwrap());

pub fn strip_ansi(s: &str) -> String {
    ANSI.replace_all(s, "").into_owned()
}

pub fn strip_version(name: &str) -> &str {
    VERSION
        .captures(name)
        .and_then(|c| c.get(1))
        .map_or(name, |m| m.as_str())
}

/// The line of a nix error that says what went wrong. Messages open with a
/// bare "error:" and a trace of "… while evaluating" frames; a failed assertion
/// lists its reasons as "- " items after "Failed assertions:".
pub fn headline(msg: &str) -> String {
    let msg = strip_ansi(msg);
    let lines: Vec<&str> = msg.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let informative = |l: &&str| {
        !l.starts_with('\u{2026}')
            && !l.starts_with("at ")
            && !l.contains('|')
            && *l != "error:"
            && !l.starts_with("(stack trace truncated")
    };
    if let Some(i) = lines.iter().position(|l| l.ends_with("Failed assertions:"))
        && let Some(item) = lines[i + 1..].iter().find(|l| l.starts_with("- "))
    {
        return item.trim_start_matches("- ").to_string();
    }
    let last = lines.iter().rev().find(|l| informative(l)).copied().unwrap_or("");
    last.strip_prefix("error: ").unwrap_or(last).to_string()
}

/// Packages an evaluation error passed through on its way down, innermost
/// first: services from option paths (`systemd.services.open-webui...` names
/// open-webui) and derivations. When the package that broke is not pinnable
/// itself, e.g. a Python library, one of these usually is.
pub fn eval_context(messages: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for msg in messages {
        let msg = strip_ansi(msg);
        let mut found: Vec<(usize, String)> = TRACE_OPTION
            .captures_iter(&msg)
            .map(|c| (c.get(0).unwrap().start(), c[1].to_string()))
            .chain(
                TRACE_DRV
                    .captures_iter(&msg)
                    .map(|c| (c.get(0).unwrap().start(), strip_version(&c[1]).to_string())),
            )
            .collect();
        found.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        for (_, name) in found {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
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
    fn headline_skips_the_trace() {
        let assertion = "error:\n       \u{2026} while calling the 'head' builtin\n         at /nix/store/x/lib/attrsets.nix:1711:13:\n         1710|           if x then\n\n       error:\n       Failed assertions:\n       - The option definition `boot.initrd.systemd.enable' in `/x/boot-unlock.nix' no longer has any effect; please remove it.\n       Scripted initrd has been removed.\n";
        assert_eq!(
            headline(assertion),
            "The option definition `boot.initrd.systemd.enable' in `/x/boot-unlock.nix' no longer has any effect; please remove it."
        );
        let plain = "error:\n       \u{2026} while evaluating the attribute 'x'\n\n       (stack trace truncated; use '--show-trace' to show the full, detailed trace)\n\n       error: undefined variable 'foo'\n";
        assert_eq!(headline(plain), "undefined variable 'foo'");
        assert_eq!(headline("error: something broke"), "something broke");
    }

    #[test]
    fn eval_context_innermost_first() {
        let msg = "error:\n       \u{2026} while evaluating the option `system.build.toplevel':\n       \u{2026} while evaluating the option `warnings':\n       \u{2026} while evaluating the option `systemd.services.open-webui.serviceConfig':\n       \u{2026} while evaluating derivation 'open-webui-0.12.3'\n       error: Refusing to evaluate package 'python3.14-posthog-7.71.0' in /nix/store/x-source/pkgs/development/python-modules/posthog/default.nix:80 because it has problems:\n".to_string();
        assert_eq!(eval_context(&[msg.clone()]), vec!["open-webui".to_string()]);
        assert_eq!(
            parse(&[msg]),
            vec![Failure::Eval { attr: "python3.14-posthog".into(), kind: EvalKind::Refused }]
        );
        let quoted = "\u{2026} while evaluating the option `services.\"nginx\".virtualHosts':\n\u{2026} while evaluating the option `systemd.user.services.foo.serviceConfig':".to_string();
        assert_eq!(eval_context(&[quoted]), vec!["foo".to_string(), "nginx".to_string()]);
    }

    #[test]
    fn version_stripping() {
        assert_eq!(strip_version("obs-studio-31.0.1"), "obs-studio");
        assert_eq!(strip_version("hello"), "hello");
        assert_eq!(strip_version("python3.13-requests-2.32"), "python3.13-requests");
    }
}
