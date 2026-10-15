//! How Codex CLI is pointed at wally's Responses translator: `-c` overrides
//! on its command line and a model catalog file, never the person's own
//! `~/.codex/config.toml`.
//!
//! `-c` outranks the root config and any profile (checked against a copy of a
//! real config that pins its own `model_provider`), so a launch needs nothing
//! written into Codex's home and leaves nothing behind in it.

use serde_json::{json, Value};

use super::catalog_models::CatalogModel;

/// The provider id the overrides define. Not one of Codex's reserved built-in
/// provider ids.
pub const CODEX_PROVIDER_ID: &str = "wally";

/// The variable Codex reads the loopback token from (`env_key`).
pub const CODEX_KEY_VARIABLE: &str = "WALLY_CODEX_API_KEY";

/// The levels a catalog entry offers in Codex's effort picker.
const REASONING_LEVELS: &[(&str, &str)] = &[
    ("none", "No reasoning"),
    ("low", "Quick reasoning"),
    ("medium", "Balanced reasoning"),
    ("high", "Thorough reasoning"),
    ("xhigh", "Extra thorough reasoning"),
    ("max", "The most reasoning the model allows"),
];

/// The window declared when the catalog does not carry one.
const FALLBACK_CONTEXT_WINDOW: i64 = 128_000;

/// A TOML string for a `-c` value or a settings line, written as a literal string so it needs no
/// double quote. On Windows Codex is npm's `codex.cmd`, and a double quote
/// cannot be passed safely through cmd.exe, so the launcher refuses one
/// (`build_batch_command_line`); a literal string also keeps a Windows path's
/// backslashes as they are. A value with a `'` in it (a home folder such as
/// `o'brien`) takes the multi-line literal form, which may hold one or two in
/// a row. Only a value with three in a row or a control character, which none
/// of ours can have, falls back to a basic string.
pub(crate) fn toml_string(value: &str) -> String {
    if value.chars().any(char::is_control) || value.contains("'''") {
        serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
    } else if value.contains('\'') {
        format!("'''{value}'''")
    } else {
        format!("'{value}'")
    }
}

/// One `model_catalog_json` entry. Codex has no metadata for a model it did not ship, and without an
/// entry it warns and falls back to defaults that fit no model here.
fn catalog_entry(model: &CatalogModel, priority: usize) -> Value {
    let context_window = if model.context_window > 0 {
        model.context_window
    } else {
        FALLBACK_CONTEXT_WINDOW
    };
    let modalities = if model.image_input {
        json!(["text", "image"])
    } else {
        json!(["text"])
    };
    json!({
        "slug": model.id,
        "display_name": model.id,
        "context_window": context_window,
        "shell_type": "default",
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "truncation_policy": {"mode": "tokens", "limit": 10000},
        "input_modalities": modalities,
        "base_instructions": "",
        "support_verbosity": false,
        "default_verbosity": "low",
        "supports_parallel_tool_calls": true,
        "supports_reasoning_summaries": true,
        // What Codex's effort picker offers. Every hosted model takes all of
        // these as `reasoning_effort`; `none` turns reasoning off. No default:
        // Codex then sends the person's own setting, or nothing, and the
        // model keeps its own default.
        "supported_reasoning_levels": REASONING_LEVELS
            .iter()
            .map(|(effort, description)| json!({"effort": effort, "description": description}))
            .collect::<Vec<_>>(),
        "default_reasoning_level": Value::Null,
        // The only value Codex 0.162 accepts. Leaving it out leaves Codex with no apply_patch tool at all and the model editing
        // files through shell heredocs; the translator carries the freeform
        // tool as a function and back.
        "apply_patch_tool_type": "freeform",
        "experimental_supported_tools": [],
    })
}

/// The `model_catalog_json` document: every catalog model, the launched one
/// first so it stays the default.
pub fn codex_model_catalog(models: &[CatalogModel]) -> String {
    let entries: Vec<Value> = models
        .iter()
        .enumerate()
        .map(|(priority, model)| catalog_entry(model, priority))
        .collect();
    json!({"models": entries}).to_string()
}

/// The `-c` overrides that send Codex to `base_url` (the translator, ending
/// in `/v1`). `catalog_path` is the file `codex_model_catalog` was written to,
/// when it could be; `context_window` is 0 when unknown.
pub fn codex_overrides(
    base_url: &str,
    catalog_path: Option<&str>,
    context_window: i64,
) -> Vec<String> {
    codex_settings(base_url, catalog_path, context_window)
        .into_iter()
        .flat_map(|setting| ["-c".to_string(), setting])
        .collect()
}

/// The settings behind `codex_overrides`, each a root-level TOML line
/// (`key=value`), for a Codex that reads them from a file instead (the
/// desktop app takes no `-c`).
pub fn codex_settings(
    base_url: &str,
    catalog_path: Option<&str>,
    context_window: i64,
) -> Vec<String> {
    let provider = CODEX_PROVIDER_ID;
    let mut settings = vec![
        format!("model_provider={}", toml_string(provider)),
        format!(
            "model_providers.{provider}={{name={}, base_url={}, wire_api='responses', env_key={}, supports_websockets=false, capabilities={{remote_compaction='unsupported'}}}}",
            toml_string("RunAnywhere"),
            toml_string(base_url),
            toml_string(CODEX_KEY_VARIABLE),
        ),
        // The translator reads compressed bodies too, but there is nothing to
        // save on a loopback hop and decompressing is time on every turn.
        "features.enable_request_compression=false".to_string(),
        // Hosted search has no chat-completions equivalent; asking for it
        // would only have the tool dropped on every request.
        "web_search='disabled'".to_string(),
    ];
    if let Some(path) = catalog_path {
        settings.push(format!("model_catalog_json={}", toml_string(path)));
    }
    if context_window > 0 {
        settings.push(format!("model_context_window={context_window}"));
    }
    settings
}

/// Whether the person chose a model on Codex's own command line.
fn names_model(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "-m" || a == "--model" || a.starts_with("--model="))
}

/// The Codex subcommands that take `-c` of their own. A `-c` given after one
/// of these replaces every `-c` given before it (checked against 0.162.1), so
/// wally's overrides go after the subcommand, or a person's own
/// `exec -c ...` would silently send the session to OpenAI. `review` takes no
/// `-m`, so its model goes in as a setting.
const SESSION_SUBCOMMANDS: &[(&str, bool)] = &[
    ("exec", true),
    ("e", true),
    ("resume", true),
    ("fork", true),
    ("review", false),
];

/// Codex's argv: the overrides and `-m model` (unless they named their own),
/// right after the subcommand when their arguments start with one that takes
/// its own `-c`, else ahead of everything; then the rest of their arguments.
pub fn codex_argv(overrides: &[String], model: &str, args: &[String]) -> Vec<String> {
    let subcommand = args
        .first()
        .and_then(|first| SESSION_SUBCOMMANDS.iter().find(|(name, _)| first == name));
    let mut ours = overrides.to_vec();
    if !model.is_empty() && !names_model(args) {
        match subcommand {
            Some((_, false)) => {
                ours.push("-c".to_string());
                ours.push(format!("model={}", toml_string(model)));
            }
            _ => {
                ours.push("-m".to_string());
                ours.push(model.to_string());
            }
        }
    }
    let mut argv = Vec::with_capacity(ours.len() + args.len());
    match subcommand {
        Some(_) => {
            argv.push(args[0].clone());
            argv.extend(ours);
            argv.extend(args[1..].iter().cloned());
        }
        None => {
            argv.extend(ours);
            argv.extend(args.iter().cloned());
        }
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn overrides_point_codex_at_the_translator_without_touching_its_config() {
        let argv = codex_overrides("http://127.0.0.1:5000/v1", Some("/tmp/models.json"), 200000);
        let settings: Vec<&String> = argv.iter().skip(1).step_by(2).collect();
        assert!(argv.iter().step_by(2).all(|flag| flag == "-c"));
        assert_eq!(settings[0], "model_provider='wally'");
        assert!(settings[1].starts_with("model_providers.wally={"));
        assert!(settings[1].contains("base_url='http://127.0.0.1:5000/v1'"));
        assert!(settings[1].contains("wire_api='responses'"));
        assert!(settings[1].contains("env_key='WALLY_CODEX_API_KEY'"));
        assert!(settings[1].contains("supports_websockets=false"));
        assert!(settings[1].contains("remote_compaction='unsupported'"));
        assert!(settings.contains(&&"web_search='disabled'".to_string()));
        assert!(settings.contains(&&"model_catalog_json='/tmp/models.json'".to_string()));
        // Codex on Windows is a .cmd shim, which cannot be handed a double quote.
        assert!(!argv.iter().any(|a| a.contains('"')), "{argv:?}");
        assert!(settings.contains(&&"model_context_window=200000".to_string()));
    }

    #[test]
    fn overrides_leave_out_what_is_not_known() {
        let argv = codex_overrides("http://127.0.0.1:5000/v1", None, 0);
        assert!(!argv.iter().any(|a| a.starts_with("model_catalog_json")));
        assert!(!argv.iter().any(|a| a.starts_with("model_context_window")));
    }

    #[test]
    fn a_windows_path_keeps_its_backslashes_in_a_literal_string() {
        let argv = codex_overrides("http://x/v1", Some("C:\\Users\\san\\m.json"), 0);
        assert!(argv.contains(&"model_catalog_json='C:\\Users\\san\\m.json'".to_string()));
    }

    #[test]
    fn a_path_with_an_apostrophe_is_a_multi_line_literal_string() {
        let argv = codex_overrides("http://x/v1", Some("C:\\Users\\o'brien\\m.json"), 0);
        assert!(argv.contains(&"model_catalog_json='''C:\\Users\\o'brien\\m.json'''".to_string()));
    }

    /// What the Windows launcher would actually run: with an apostrophe in
    /// the home folder and a backslashed path, every argument passes the
    /// check `codex.cmd` is launched through.
    #[test]
    fn the_overrides_pass_the_windows_cmd_launcher() {
        let argv = codex_argv(
            &codex_overrides(
                "http://127.0.0.1:5000/v1",
                Some("C:\\Users\\o'brien\\AppData\\Local\\RunAnywhere\\state\\codex\\m.json"),
                1_048_567,
            ),
            "glm-5.3-flash",
            &strings(&["exec", "hi"]),
        );
        assert!(
            crate::harness::build_batch_command_line("C:\\npm\\codex.cmd", &argv).is_ok(),
            "{argv:?}"
        );
    }

    #[test]
    fn the_model_goes_ahead_of_their_arguments_unless_they_named_one() {
        let overrides = strings(&["-c", "x=1"]);
        assert_eq!(
            codex_argv(&overrides, "glm-5.3-flash", &strings(&["--full-auto"])),
            strings(&["-c", "x=1", "-m", "glm-5.3-flash", "--full-auto"])
        );
        for theirs in [
            &["-m", "other"][..],
            &["--model", "other"],
            &["--model=other"],
        ] {
            let argv = codex_argv(&overrides, "glm-5.3-flash", &strings(theirs));
            assert!(!argv.contains(&"glm-5.3-flash".to_string()), "{argv:?}");
        }
    }

    /// Codex drops every `-c` before a subcommand once the subcommand has a
    /// `-c` of its own, so ours follow the subcommand.
    #[test]
    fn the_overrides_follow_a_subcommand_that_takes_its_own() {
        let overrides = strings(&["-c", "model_provider='wally'"]);
        assert_eq!(
            codex_argv(
                &overrides,
                "glm-5.3-flash",
                &strings(&["exec", "-c", "model_reasoning_effort=low", "hi"])
            ),
            strings(&[
                "exec",
                "-c",
                "model_provider='wally'",
                "-m",
                "glm-5.3-flash",
                "-c",
                "model_reasoning_effort=low",
                "hi"
            ])
        );
        assert_eq!(
            codex_argv(&overrides, "glm-5.3-flash", &strings(&["resume", "--last"]))[..5],
            strings(&[
                "resume",
                "-c",
                "model_provider='wally'",
                "-m",
                "glm-5.3-flash"
            ])
        );
        assert_eq!(
            codex_argv(&overrides, "glm-5.3-flash", &strings(&["review"])),
            strings(&[
                "review",
                "-c",
                "model_provider='wally'",
                "-c",
                "model='glm-5.3-flash'"
            ])
        );
    }

    #[test]
    fn the_catalog_lists_every_model_launched_first_with_its_window() {
        let models = vec![
            CatalogModel {
                id: "glm-5.3-flash".to_string(),
                context_window: 200000,
                image_input: true,
                ..Default::default()
            },
            CatalogModel {
                id: "mimo-v2.6-pro".to_string(),
                ..Default::default()
            },
        ];
        let doc: Value = serde_json::from_str(&codex_model_catalog(&models)).unwrap();
        let entries = doc["models"].as_array().unwrap();
        assert_eq!(entries[0]["slug"], "glm-5.3-flash");
        assert_eq!(entries[0]["priority"], 0);
        assert_eq!(entries[0]["context_window"], 200000);
        assert_eq!(entries[0]["input_modalities"], json!(["text", "image"]));
        assert_eq!(entries[0]["apply_patch_tool_type"], "freeform");
        let levels: Vec<&str> = entries[0]["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|level| level["effort"].as_str().unwrap())
            .collect();
        assert_eq!(levels, ["none", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(entries[1]["slug"], "mimo-v2.6-pro");
        assert_eq!(entries[1]["context_window"], 128000);
        assert_eq!(entries[1]["input_modalities"], json!(["text"]));
    }
}
