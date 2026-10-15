//! How the ChatGPT desktop app's Codex is pointed at wally's Responses
//! translator.
//!
//! The app takes no `-c` overrides, so the settings `wally codex` passes on
//! the command line go into the `config.toml` of a Codex home wally owns for
//! the app, as one block wally rewrites on every launch. The rest of that
//! file is the app's own (project trust, toggles) and is kept. The reader's
//! `~/.codex` and their own ChatGPT app are never touched.

use super::codex::{codex_settings, toml_string};

/// The bundle id of the ChatGPT app that carries Codex. The older chat-only
/// app has a different one and no Codex to point anywhere.
pub const CHATGPT_BUNDLE_ID: &str = "com.openai.codex";

const BLOCK_BEGIN: &str = "# wally: managed settings, rewritten on every `wally chatgpt` launch";
const BLOCK_END: &str = "# wally: end of managed settings";

/// The block of root-level settings for `model` behind the translator at
/// `base_url` (ending in `/v1`).
pub fn chatgpt_settings(
    model: &str,
    base_url: &str,
    catalog_path: Option<&str>,
    context_window: i64,
) -> Vec<String> {
    let mut settings = vec![format!("model={}", toml_string(model))];
    settings.extend(codex_settings(base_url, catalog_path, context_window));
    settings
}

/// The key a root-level `key=value` line sets, or None for anything else.
fn root_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    Some(key.trim())
}

/// `existing` (the home's `config.toml`, empty when there is none) with the
/// managed block replaced by `settings`.
///
/// The block goes first, because root-level keys must come before any table.
/// A root-level line elsewhere that sets one of the same keys is dropped, so
/// the file never names a key twice, which TOML refuses.
pub fn chatgpt_config(existing: &str, settings: &[String]) -> String {
    let ours: Vec<&str> = settings.iter().filter_map(|line| root_key(line)).collect();
    let mut kept = Vec::new();
    let mut in_block = false;
    let mut in_table = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_BEGIN {
            in_block = true;
            continue;
        }
        if in_block {
            if trimmed == BLOCK_END {
                in_block = false;
            }
            continue;
        }
        if trimmed.starts_with('[') {
            in_table = true;
        }
        if !in_table && root_key(line).is_some_and(|key| ours.contains(&key)) {
            continue;
        }
        kept.push(line);
    }
    while kept.first().is_some_and(|line| line.trim().is_empty()) {
        kept.remove(0);
    }
    let mut out = String::new();
    out.push_str(BLOCK_BEGIN);
    out.push('\n');
    for line in settings {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(BLOCK_END);
    out.push('\n');
    if !kept.is_empty() {
        out.push('\n');
        out.push_str(&kept.join("\n"));
        out.push('\n');
    }
    out
}

/// The `auth.json` that lets the app's Codex start without a ChatGPT sign-in:
/// API-key mode, with a key nothing reads. Requests go to the wally provider,
/// which takes its key from `WALLY_CODEX_API_KEY` instead. Only ever written
/// into wally's own home, and only when that home has none.
pub fn chatgpt_auth() -> &'static str {
    "{\n  \"OPENAI_API_KEY\": \"wally-local\",\n  \"auth_mode\": \"apikey\"\n}\n"
}

/// The app's first-run questions ("Which best describes your work?") are a
/// one-time onboarding it records in the Codex home's
/// `.codex-global-state.json`. wally's home is new to the app, so it would ask
/// on the first launch; this marks onboarding as skipped, in coding mode, the
/// state the app itself writes when the person skips it.
///
/// `existing` is that file's text, empty when there is none. Only the
/// onboarding keys are set; everything else the app wrote is kept. None when
/// the file is not a JSON object, which is then left for the app to deal with
/// rather than replaced.
pub fn with_onboarding_done(existing: &str, now_unix: i64) -> Option<String> {
    let mut state: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(existing).ok()?
    };
    let root = state.as_object_mut()?;
    let atoms = root
        .entry("electron-persisted-atom-state")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()?;
    let welcome = atoms
        .entry("electron:onboarding-welcome-v2-role-state")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()?;
    for (key, value) in [
        ("completedConversationalOnboarding", serde_json::json!(true)),
        ("conversationalOnboardingSkipped", serde_json::json!(true)),
        ("workMode", serde_json::json!("coding")),
        ("personalizedSuggestionsEnabled", serde_json::json!(false)),
    ] {
        welcome.entry(key).or_insert(value);
    }
    atoms
        .entry("electron:onboarding-projectless-completed")
        .or_insert(serde_json::json!(true));
    atoms
        .entry("last_completed_onboarding")
        .or_insert(serde_json::json!(now_unix));
    serde_json::to_string_pretty(&state).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Vec<String> {
        chatgpt_settings(
            "glm-5.3-flash",
            "http://127.0.0.1:5000/v1",
            Some("/state/models.json"),
            1_048_567,
        )
    }

    #[test]
    fn a_fresh_home_gets_the_block_alone() {
        let config = chatgpt_config("", &settings());
        assert!(config.starts_with(BLOCK_BEGIN));
        assert!(config.contains("model='glm-5.3-flash'\n"));
        assert!(config.contains("model_provider='wally'\n"));
        assert!(config.contains("base_url='http://127.0.0.1:5000/v1'"));
        assert!(config.contains("model_catalog_json='/state/models.json'\n"));
        assert!(config.trim_end().ends_with(BLOCK_END));
    }

    #[test]
    fn the_apps_own_settings_survive_and_the_block_is_replaced() {
        let first = chatgpt_config("", &settings());
        let app_wrote =
            format!("{first}\n[projects.\"/Users/me/repo\"]\ntrust_level = \"trusted\"\n");
        let next = chatgpt_settings("mimo-v2.6-pro", "http://127.0.0.1:6000/v1", None, 0);
        let second = chatgpt_config(&app_wrote, &next);
        assert_eq!(second.matches(BLOCK_BEGIN).count(), 1);
        assert!(second.contains("model='mimo-v2.6-pro'"));
        assert!(!second.contains("glm-5.3-flash"));
        assert!(!second.contains("5000"));
        assert!(second.contains("[projects.\"/Users/me/repo\"]\ntrust_level = \"trusted\""));
    }

    #[test]
    fn a_root_key_the_app_set_outside_the_block_is_not_left_twice() {
        let existing =
            "model = \"gpt-5\"\nnotify = [\"x\"]\n\n[tui]\nmodel = \"kept, a table key\"\n";
        let config = chatgpt_config(existing, &settings());
        let root: Vec<&str> = config
            .lines()
            .take_while(|line| !line.trim_start().starts_with('['))
            .filter_map(root_key)
            .collect();
        assert_eq!(root.iter().filter(|key| **key == "model").count(), 1);
        assert!(root.contains(&"notify"));
        assert!(config.contains("[tui]\nmodel = \"kept, a table key\""));
    }

    #[test]
    fn the_auth_file_is_api_key_mode_with_no_real_key() {
        let auth: serde_json::Value = serde_json::from_str(chatgpt_auth()).unwrap();
        assert_eq!(auth["auth_mode"], "apikey");
        assert_eq!(auth["OPENAI_API_KEY"], "wally-local");
    }

    #[test]
    fn onboarding_is_marked_skipped_in_a_new_home() {
        let state: serde_json::Value =
            serde_json::from_str(&with_onboarding_done("", 100).unwrap()).unwrap();
        let atoms = &state["electron-persisted-atom-state"];
        let welcome = &atoms["electron:onboarding-welcome-v2-role-state"];
        assert_eq!(welcome["completedConversationalOnboarding"], true);
        assert_eq!(welcome["conversationalOnboardingSkipped"], true);
        assert_eq!(welcome["workMode"], "coding");
        assert_eq!(atoms["electron:onboarding-projectless-completed"], true);
        assert_eq!(atoms["last_completed_onboarding"], 100);
    }

    #[test]
    fn what_the_app_wrote_is_kept_and_its_choices_win() {
        let existing = r#"{"desktop-first-seen-at-ms": 5, "electron-persisted-atom-state": {"x": 1, "electron:onboarding-welcome-v2-role-state": {"workMode": "writing"}, "last_completed_onboarding": 7}}"#;
        let state: serde_json::Value =
            serde_json::from_str(&with_onboarding_done(existing, 100).unwrap()).unwrap();
        assert_eq!(state["desktop-first-seen-at-ms"], 5);
        let atoms = &state["electron-persisted-atom-state"];
        assert_eq!(atoms["x"], 1);
        assert_eq!(
            atoms["electron:onboarding-welcome-v2-role-state"]["workMode"],
            "writing"
        );
        assert_eq!(atoms["last_completed_onboarding"], 7);
    }

    #[test]
    fn a_file_that_is_not_a_json_object_is_left_alone() {
        assert_eq!(with_onboarding_done("not json", 1), None);
        assert_eq!(with_onboarding_done("[1]", 1), None);
    }
}
