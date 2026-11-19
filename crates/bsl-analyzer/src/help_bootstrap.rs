//! Platform help selection for every command that analyzes or serves platform
//! docs. It runs before the first platform lookup, so the configured source —
//! not the build default — serves the whole process.

use std::path::{Path, PathBuf};

use project_model::ProjectConfig;

/// Selects the platform help of the project at `root` (or of the explicit config
/// file). An unreadable config leaves the build default in place; the command
/// reports the config error itself when it loads the project.
pub fn bootstrap_for_root(root: &Path, explicit_config: Option<&Path>) {
    let config = match explicit_config {
        Some(path) => ProjectConfig::load_from_file(path).map(Some),
        None => ProjectConfig::load(root),
    };
    match config {
        Ok(config) => {
            platform_help::bootstrap(config.as_ref(), root);
        }
        Err(error) => {
            tracing::warn!(%error, "project config unreadable; platform help uses the default source");
            platform_help::bootstrap(None, root);
        }
    }
}

/// Selects the platform help from an already loaded project config.
pub fn bootstrap_for_config(config: &ProjectConfig, root: &Path) {
    platform_help::bootstrap(Some(config), root);
}

/// Selects the platform help of the project at `root` (without a root: of no project, from
/// the current directory) on a thread of its own, and returns at once.
///
/// For a server whose clients give up on the handshake long before a first extraction of an
/// installed platform's help ends. The load is announced before the thread starts, so no
/// lookup in between can fix the build default: a lookup waits for the load, and a caller
/// that must not wait asks `bsl_platform::platform_help_pending` first.
pub fn spawn_bootstrap(root: Option<PathBuf>) {
    let load = bsl_platform::begin_platform_help_load();
    let thread_root = root.clone();
    let spawned =
        std::thread::Builder::new().name("bsl-platform-help".to_owned()).spawn(move || {
            let _load = load;
            bootstrap_at(thread_root.as_deref());
        });
    if let Err(error) = spawned {
        // The announcement ended with the closure, and nothing has read the help yet: the
        // selection is made here instead, at the price of the wait it was meant to spare.
        tracing::warn!(%error, "platform help loader did not start; loading the help in place");
        bootstrap_at(root.as_deref());
    }
}

fn bootstrap_at(root: Option<&Path>) {
    match root {
        Some(root) => bootstrap_for_root(root, None),
        None => {
            platform_help::bootstrap(None, &std::env::current_dir().unwrap_or_default());
        }
    }
}
