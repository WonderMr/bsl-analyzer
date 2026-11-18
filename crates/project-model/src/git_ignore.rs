//! What the git work tree holding a source root leaves out below it.
//!
//! A checkout carries directories that are no part of what was checked out: build
//! output, tool state and, where tools such as Claude Code keep them
//! (`.claude/worktrees/<name>`), linked worktrees, each a full second copy of the
//! configuration. Walked with the rest, such a copy is indexed as a second set of the
//! same objects and modules: every declaration appears twice and references are split
//! between the copies. The work tree's ignore rules and its repository's list of linked
//! worktrees name those directories; this module reads them for each source root.
//!
//! Read are the `.gitignore` at the top of the work tree and the repository's
//! `info/exclude`. A `.gitignore` in a subdirectory is not read, since finding one takes
//! a walk of the tree, and neither is the user's global excludes file, which would make
//! the project differ between machines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use stdx::path_exclusion::{GitIgnored, WorkTreeRules};

/// What git leaves out below each of `roots`.
pub(crate) fn below(roots: &[PathBuf]) -> GitIgnored {
    let mut ignored = GitIgnored::default();
    let mut rules_by_top: HashMap<PathBuf, Arc<WorkTreeRules>> = HashMap::new();
    for root in roots {
        let found = std::path::absolute(root).ok().and_then(|absolute| {
            let (top, git_dir) = work_tree_of(&absolute)?;
            let relative = absolute.strip_prefix(&top).ok()?.to_path_buf();
            Some((top, git_dir, relative))
        });
        let Some((top, git_dir, relative)) = found else {
            ignored.add_root(root, None);
            continue;
        };
        tracing::debug!(?root, ?top, "source root lies in a git work tree");
        let rules = rules_by_top
            .entry(top)
            .or_insert_with_key(|top| Arc::new(rules_of(top, &git_dir)))
            .clone();
        ignored.add_root(root, Some((rules, relative.as_path())));
    }
    ignored
}

/// The top of the git work tree holding `root` (the nearest directory at or above it
/// with a `.git` entry) and the git directory of that work tree.
fn work_tree_of(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let top = root.ancestors().find(|dir| dir.join(".git").exists())?;
    let dot_git = top.join(".git");
    let git_dir = if dot_git.is_dir() { dot_git } else { linked_git_dir(top, &dot_git)? };
    Some((top.to_path_buf(), git_dir))
}

/// The git directory a `.git` file names (`gitdir: <path>`, relative to the work tree
/// when not absolute), as a linked worktree or a submodule carries it.
fn linked_git_dir(top: &Path, dot_git: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dot_git).ok()?;
    let path = text.trim().strip_prefix("gitdir:")?.trim();
    Some(top.join(path))
}

/// The directory every worktree of a repository shares: a linked worktree's git
/// directory names it in `commondir`, relative to itself.
fn common_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => git_dir.join(text.trim()),
        Err(_) => git_dir.to_path_buf(),
    }
}

fn rules_of(top: &Path, git_dir: &Path) -> WorkTreeRules {
    let common = common_dir(git_dir);
    let ignore_files = [top.join(".gitignore"), common.join("info").join("exclude")];
    WorkTreeRules::new(top, &ignore_files, &linked_worktrees(&common, top))
}

/// The repository's linked worktrees below `top`, relative to it. Each is listed in
/// `worktrees/<id>/gitdir` of the common directory by the path of its `.git` file.
fn linked_worktrees(common: &Path, top: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else { return Vec::new() };
    let Ok(top) = stdx::path_exclusion::resolve(top) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let admin = entry.path();
            let text = std::fs::read_to_string(admin.join("gitdir")).ok()?;
            // Absolute as git writes it by default; a relative path is taken from `admin`.
            let dot_git = admin.join(text.trim());
            // A worktree removed without `git worktree prune` stays listed; nothing of it
            // is left to walk.
            let worktree = stdx::path_exclusion::resolve(dot_git.parent()?).ok()?;
            let relative = worktree.strip_prefix(&top).ok()?;
            if relative.as_os_str().is_empty() {
                None
            } else {
                Some(relative.to_path_buf())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use crate::{ExcludedPaths, Project, SourceSet};

    const CONFIGURATION: &str = "<MetaDataObject><Configuration></Configuration></MetaDataObject>";
    const MODULE: &str = "CommonModules/Common/Ext/Module.bsl";

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A configuration with one module at `dir`.
    fn configuration(dir: &Path) {
        write(&dir.join("Configuration.xml"), CONFIGURATION);
        write(&dir.join(MODULE), "Процедура Тест() Экспорт\nКонецПроцедуры\n");
    }

    /// The `.bsl` files one scan of the project's roots reaches, relative to `top`.
    fn scanned(project: &Project, top: &Path) -> Vec<String> {
        let set =
            SourceSet::scan_in_scope(&project.source_roots(), &[], project.source_exclusions());
        let mut files: Vec<String> = set
            .files
            .iter()
            .filter(|file| file.walked.extension().is_some_and(|extension| extension == "bsl"))
            .map(|file| {
                let relative = file.walked.strip_prefix(top).unwrap();
                relative.to_string_lossy().replace('\\', "/")
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn a_git_ignored_copy_of_the_configuration_is_not_a_source() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        fs::create_dir_all(top.join(".git")).unwrap();
        configuration(top);
        let copy = top.join(".claude/worktrees/copy");
        configuration(&copy);
        let copied = format!(".claude/worktrees/copy/{MODULE}");

        let before = Project::new(top).unwrap();
        assert_eq!(scanned(&before, top), [copied.clone(), MODULE.to_owned()]);
        assert_eq!(before.source_exclusions(), &ExcludedPaths::default());

        write(&top.join(".gitignore"), "# local tool state\n.claude/\n");
        let project = Project::new(top).unwrap();
        assert_eq!(project.source_roots(), [top.to_path_buf()]);
        assert_eq!(scanned(&project, top), [MODULE]);
        assert!(project.is_source_excluded(&copy.join(MODULE)));
        assert!(!project.is_source_excluded(&top.join(MODULE)));

        write(&top.join("bsl-analyzer.toml"), "[source]\nrespect_gitignore = false\n");
        let switched_off = Project::new(top).unwrap();
        assert_eq!(scanned(&switched_off, top), [copied, MODULE.to_owned()]);
    }

    #[test]
    fn info_exclude_is_read_like_the_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        write(&top.join(".git/info/exclude"), "/build/\n");
        configuration(top);
        configuration(&top.join("build/copy"));

        let project = Project::new(top).unwrap();
        assert_eq!(scanned(&project, top), [MODULE]);
    }

    #[test]
    fn a_linked_worktree_inside_the_project_is_not_a_source() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        configuration(top);
        let worktree = top.join("nested/copy");
        configuration(&worktree);
        // The layout `git worktree add nested/copy` leaves behind: the worktree's `.git`
        // file names its git directory, and the repository lists the worktree there.
        let admin = top.join(".git/worktrees/copy");
        write(&admin.join("commondir"), "../..\n");
        write(&admin.join("gitdir"), &format!("{}\n", worktree.join(".git").display()));
        write(&worktree.join(".git"), &format!("gitdir: {}\n", admin.display()));

        let project = Project::new(top).unwrap();
        assert_eq!(scanned(&project, top), [MODULE]);
        assert!(project.is_source_excluded(&worktree.join(MODULE)));

        // Opened on its own, the worktree is the project, and nothing of it is left out.
        let opened = Project::new(&worktree).unwrap();
        assert_eq!(scanned(&opened, &worktree), [MODULE]);
    }

    #[test]
    fn a_declared_root_inside_an_ignored_directory_keeps_its_sources() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        fs::create_dir_all(top.join(".git")).unwrap();
        write(&top.join(".gitignore"), "build/\n");
        configuration(&top.join("build/cf"));
        write(&top.join("bsl-analyzer.toml"), "[source]\nroot = \"build/cf\"\n");

        let project = Project::new(top).unwrap();
        assert_eq!(scanned(&project, top), [format!("build/cf/{MODULE}")]);
    }

    #[test]
    fn a_project_outside_git_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        write(&top.join(".gitignore"), ".claude/\n");
        configuration(top);
        configuration(&top.join(".claude/worktrees/copy"));

        let project = Project::new(top).unwrap();
        assert_eq!(project.source_exclusions(), &ExcludedPaths::default());
        assert_eq!(scanned(&project, top).len(), 2);
    }
}
