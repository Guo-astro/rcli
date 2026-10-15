//! `wally codex`: Codex CLI against a local or hosted model, through the
//! loopback Responses translator (`crate::codex`).
//!
//! Its own command rather than a row in `cmd_editors`' table: that table is the
//! Anthropic-shaped tools, and Codex is wired by command-line overrides, not
//! environment variables.

use std::path::{Path, PathBuf};

use crate::bootstrap::GlobalOptions;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::codex;
use crate::commands::cmd_editors::{
    cloud_context_window, context_window_for_tool, context_window_problem,
    copy_recursive_overwrite, text_only_ids, write_private, RemovedOnDrop, ScopedEnv,
};
use crate::commands::resolve_default_model;
use crate::config::cli_paths;
use crate::harness::{self, codex as codex_config};
use crate::io::output as out;
use crate::util::getenv;

const COMMAND: &str = "codex";

/// The model catalog Codex reads at startup, in a file wally owns for this
/// session only. None when there is nowhere to write it; Codex then runs on
/// its fallback metadata, which works but warns.
fn write_model_catalog(catalog: &[harness::CatalogModel]) -> RemovedOnDrop {
    let state_dir = cli_paths::state_dir();
    if state_dir.is_empty() {
        return RemovedOnDrop(None);
    }
    let dir = Path::new(&state_dir).join("codex");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        out::status_line(&format!("could not write Codex's model catalog ({error})"));
        return RemovedOnDrop(None);
    }
    let path = dir.join(format!("wally-models-{}.json", std::process::id()));
    match write_private(&path, &codex_config::codex_model_catalog(catalog)) {
        Ok(()) => RemovedOnDrop(Some(path.to_string_lossy().into_owned())),
        Err(error) => {
            out::status_line(&format!("could not write Codex's model catalog ({error})"));
            RemovedOnDrop(None)
        }
    }
}

/// What a wally Codex session takes from the reader's own Codex home: the
/// settings (MCP servers, project trust, their own provider tables, which the
/// `-c` overrides outrank), their instructions, and their skills, rules and
/// agents. Never the sign-in, the sessions, the history or the databases.
const CODEX_HOME_SEEDS: &[&str] = &["config.toml", "AGENTS.md", "skills", "rules", "agents"];

/// The reader's Codex home: `CODEX_HOME` when they set one, else `~/.codex`.
fn reader_codex_home() -> Option<PathBuf> {
    if let Some(home) = getenv("CODEX_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    #[cfg(windows)]
    let home = getenv("USERPROFILE").filter(|h| !h.is_empty());
    #[cfg(not(windows))]
    let home = getenv("HOME").filter(|h| !h.is_empty());
    home.map(|home| Path::new(&home).join(".codex"))
}

/// A Codex home wally owns, seeded from the reader's on every launch.
///
/// Codex records every thread, with its provider and model, in its home's
/// databases, and the reader's own Codex (the CLI, the IDE extension, the
/// app) lists them all. A wally session there leaves threads naming a
/// `wally` provider that exists only while wally runs, and a model the
/// reader's Codex cannot serve. Running in a home of wally's own leaves
/// nothing to undo when the session ends, however it ends -- Ctrl-C, a closed
/// terminal, SIGKILL -- because nothing was written to the reader's.
/// None without a usable state directory, in which case the reader's home is
/// used as before rather than a root-level path.
fn prepare_codex_home(state_dir: &str, reader: Option<&Path>) -> Option<PathBuf> {
    if state_dir.is_empty() {
        return None;
    }
    let ours = Path::new(state_dir).join("codex").join("home");
    std::fs::create_dir_all(&ours).ok()?;
    if let Some(reader) = reader {
        for name in CODEX_HOME_SEEDS {
            let source = reader.join(name);
            let target = ours.join(name);
            if source.is_dir() {
                // A whole refresh, so a skill the reader deleted goes too.
                let _ = std::fs::remove_dir_all(&target);
                copy_recursive_overwrite(&source, &target);
            } else if source.is_file() {
                let _ = std::fs::copy(&source, &target);
            } else {
                let _ = std::fs::remove_file(&target);
            }
        }
    }
    Some(ours)
}

/// Holds the translator open and prints what to point at it, for a client
/// that speaks the Responses API but is not Codex.
fn serve(model: &str, options: &GlobalOptions) -> i32 {
    let Some(endpoint) = harness::resolve(model, options, COMMAND) else {
        return 1;
    };
    let catalog = harness::catalog_models_for(&endpoint, model);
    let Some(shim) = codex::start(&endpoint, model, options.verbose, &text_only_ids(&catalog))
    else {
        harness::release(&endpoint);
        return 1;
    };
    out::result_line(&format!("OPENAI_BASE_URL={}", shim.base_url));
    out::result_line(&format!("OPENAI_API_KEY={}", shim.auth_token));
    out::status_line(&format!("serving {model}; press Ctrl-C to stop"));
    // No signal handling: Ctrl-C ends the process, and the OS reclaims the port
    // and the model.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn run(
    model: &str,
    args: &[String],
    context_override: Option<i64>,
    options: &GlobalOptions,
) -> i32 {
    if model.is_empty() {
        // No model named means no wiring to do: Codex runs exactly as the
        // reader has it configured. Same contract as `wally opencode`.
        return harness::launch(COMMAND, "", args, options);
    }
    let Some(endpoint) = harness::resolve(model, options, COMMAND) else {
        return 1;
    };
    // Read once: it names the models Codex lists, their windows, and which of
    // them take no images.
    let catalog = harness::catalog_models_for(&endpoint, model);
    let Some(mut shim) = codex::start(&endpoint, model, options.verbose, &text_only_ids(&catalog))
    else {
        harness::release(&endpoint);
        return 1;
    };
    out::status_line(&format!(
        "codex will talk to {model} through {}",
        shim.base_url
    ));
    let real = if endpoint.serving {
        endpoint.context_window
    } else if catalog[0].context_window > 0 {
        catalog[0].context_window
    } else {
        cloud_context_window(model)
    };
    let context = context_window_for_tool(real, context_override);
    if context > 0 {
        out::status_line(&format!("context window: {context} tokens"));
    }

    // The catalog carries each model's window too, and Codex reads it from
    // there for the launched model; a smaller asked-for window goes in both.
    let mut catalog = catalog;
    if context > 0 {
        catalog[0].context_window = context;
    }
    let catalog_file = write_model_catalog(&catalog);
    let overrides =
        codex_config::codex_overrides(&shim.base_url, catalog_file.0.as_deref(), context);
    // The loopback token, never the upstream key, and only for this child.
    let codex_home = prepare_codex_home(&cli_paths::state_dir(), reader_codex_home().as_deref());
    let status = {
        let _key = ScopedEnv::new(codex_config::CODEX_KEY_VARIABLE, &shim.auth_token);
        let _home = codex_home
            .as_ref()
            .map(|home| ScopedEnv::new("CODEX_HOME", &home.to_string_lossy()));
        harness::launch(
            COMMAND,
            "",
            &codex_config::codex_argv(&overrides, model, args),
            options,
        )
    };

    codex::stop(&mut shim);
    harness::release(&endpoint);
    status
}

pub fn register_codex(app: &mut App) {
    let command = app.add_subcommand(COMMAND, "Open Codex with a model");
    command.footer(&examples_footer(&[
        Example::new(
            "wally codex -m qwen3-4b-instruct-2507",
            "The certified local coding model",
        ),
        Example::new(
            "wally codex -m glm-5.3-flash",
            "A hosted model (needs `wally account login`)",
        ),
        Example::new(
            "wally codex -m glm-5.3-flash -- exec \"explain this project\"",
            "Pass arguments through to Codex after --",
        ),
    ]));
    command.add_option(
        "-m,--model",
        ValueType::Text,
        "A model on this machine, or a hosted one from your account",
    );
    command.add_option(
        "--context-window",
        ValueType::Int64,
        "Tell Codex the model holds this many tokens, so it compacts sooner",
    );
    command.add_flag(
        "--serve",
        "Print the endpoint and keep it open instead of launching",
    );
    // Tokens after the wally flags belong to Codex, its own flags included;
    // `run()` inserts the `--` that lets them through (SplitPassthroughArgv).
    command
        .add_option("args", ValueType::Text, "Passed through to codex")
        .multi();
    command.prefix_command(true);
    command.callback(|p, g| {
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
        if let Some(problem) = context_window_problem(p.get_i64("--context-window")) {
            out::error_line(&problem);
            return 2;
        }
        // A missing tool shows only that it is missing and how to get it,
        // before any model resolution. --serve launches nothing.
        if !p.flag("--serve") && !harness::ensure_installed(COMMAND) {
            return 127;
        }
        let effective =
            resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color);
        if p.flag("--serve") {
            if effective.is_empty() {
                out::error_line(
                    "--serve requires --model <id>, and no default is set (wally models default <id>)",
                );
                return 2;
            }
            serve(&effective, g)
        } else {
            run(
                &effective,
                &p.get_strs("args"),
                p.get_i64("--context-window"),
                g,
            )
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seeded_home_carries_settings_and_skills_but_no_threads_or_sign_in() {
        let reader = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("config.toml", "model = 'gpt'"),
            ("AGENTS.md", "rules"),
            ("auth.json", "secret"),
            ("history.jsonl", "{}"),
            ("state_5.sqlite", "db"),
        ] {
            std::fs::write(reader.path().join(name), body).unwrap();
        }
        std::fs::create_dir_all(reader.path().join("skills/one")).unwrap();
        std::fs::write(reader.path().join("skills/one/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(reader.path().join("sessions/2026")).unwrap();

        let home =
            prepare_codex_home(&state.path().to_string_lossy(), Some(reader.path())).unwrap();
        assert!(home.starts_with(state.path()));
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "model = 'gpt'"
        );
        assert!(home.join("AGENTS.md").is_file());
        assert!(home.join("skills/one/SKILL.md").is_file());
        for absent in ["auth.json", "history.jsonl", "state_5.sqlite", "sessions"] {
            assert!(!home.join(absent).exists(), "{absent}");
        }
    }

    #[test]
    fn a_skill_the_reader_deleted_leaves_the_seeded_home_too() {
        let reader = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let state_dir = state.path().to_string_lossy().into_owned();
        std::fs::create_dir_all(reader.path().join("skills/old")).unwrap();
        prepare_codex_home(&state_dir, Some(reader.path())).unwrap();
        std::fs::remove_dir_all(reader.path().join("skills/old")).unwrap();
        let home = prepare_codex_home(&state_dir, Some(reader.path())).unwrap();
        assert!(!home.join("skills/old").exists());
    }

    #[test]
    fn no_state_directory_means_no_home_of_our_own() {
        assert_eq!(prepare_codex_home("", None), None);
    }
}
