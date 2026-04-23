use std::fs;

use codex_execpolicy::MatchOptions;
use codex_execpolicy::PolicyParser;
use codex_execpolicy::format_matches_json;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

fn evaluate_policy(policy_contents: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let codex_home = TempDir::new()?;
    let policy_path = codex_home.path().join("rules").join("policy.rules");
    fs::create_dir_all(
        policy_path
            .parent()
            .expect("policy path should have a parent"),
    )?;
    fs::write(&policy_path, policy_contents)?;

    let mut parser = PolicyParser::new();
    parser.parse(
        policy_path
            .to_str()
            .expect("policy path should be valid UTF-8"),
        &fs::read_to_string(&policy_path)?,
    )?;
    let policy = parser.build();
    let matched_rules = policy.matches_for_command_with_options(
        &[
            "git".to_string(),
            "push".to_string(),
            "origin".to_string(),
            "main".to_string(),
        ],
        /*heuristics_fallback*/ None,
        &MatchOptions {
            resolve_host_executables: false,
        },
    );
    Ok(serde_json::from_str(&format_matches_json(
        &matched_rules,
        /*pretty*/ false,
    )?)?)
}

#[test]
fn execpolicy_check_matches_expected_json() -> Result<(), Box<dyn std::error::Error>> {
    let result = evaluate_policy(
        r#"
prefix_rule(
    pattern = ["git", "push"],
    decision = "forbidden",
)
"#,
    )?;

    assert_eq!(
        result,
        json!({
            "decision": "forbidden",
            "matchedRules": [
                {
                    "prefixRuleMatch": {
                        "matchedPrefix": ["git", "push"],
                        "decision": "forbidden"
                    }
                }
            ]
        })
    );

    Ok(())
}

#[test]
fn execpolicy_check_includes_justification_when_present() -> Result<(), Box<dyn std::error::Error>>
{
    let result = evaluate_policy(
        r#"
prefix_rule(
    pattern = ["git", "push"],
    decision = "forbidden",
    justification = "pushing is blocked in this repo",
)
"#,
    )?;

    assert_eq!(
        result,
        json!({
            "decision": "forbidden",
            "matchedRules": [
                {
                    "prefixRuleMatch": {
                        "matchedPrefix": ["git", "push"],
                        "decision": "forbidden",
                        "justification": "pushing is blocked in this repo"
                    }
                }
            ]
        })
    );

    Ok(())
}
