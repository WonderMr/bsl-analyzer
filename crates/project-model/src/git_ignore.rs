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
//!
//! A project is derived many times over (MCP derives it again on every incremental
//! reload), so the rules of a work tree are read and compiled once and kept with the
//! state of every file they came from; they are read again only after one of those files
//! changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use stdx::path_exclusion::{GitIgnored, WorkTreeRules};

/// What git leaves out below each of `roots`.
pub(crate) fn below(roots: &[PathBuf]) -> GitIgnored {
    let mut ignored = GitIgnored::default();
    let mut work_trees = WorkTrees::default();
    let mut rules_by_top: HashMap<PathBuf, Arc<WorkTreeRules>> = HashMap::new();
    for root in roots {
        let found = std::path::absolute(root).ok().and_then(|absolute| {
            let (top, git_dir) = work_trees.of(&absolute)?;
            let relative = absolute.strip_prefix(&top).ok()?.to_path_buf();
            Some((top, git_dir, relative))
        });
        let Some((top, git_dir, relative)) = found else {
            ignored.add_root(root, None);
            continue;
        };
        tracing::debug!(?root, ?top, "source root lies in a git work tree");
        let rules = rules_by_top.entry(top).or_insert_with_key(|top| rules_of(top, &git_dir));
        ignored.add_root(root, Some((rules.clone(), relative.as_path())));
    }
    ignored
}

/// The work trees holding the roots of one project. Roots side by side share their
/// ancestors, so each directory is probed for `.git` once rather than once per root.
#[derive(Default)]
struct WorkTrees {
    /// Whether each directory probed holds a `.git` entry.
    probed: HashMap<PathBuf, bool>,
    /// The git directory of each top found; `None` when its `.git` file names none.
    git_dirs: HashMap<PathBuf, Option<PathBuf>>,
}

impl WorkTrees {
    /// The top of the git work tree holding `root` (the nearest directory at or above it
    /// with a `.git` entry) and the git directory of that work tree.
    fn of(&mut self, root: &Path) -> Option<(PathBuf, PathBuf)> {
        let top = root.ancestors().find(|dir| self.holds_git(dir))?;
        let git_dir = self.git_dirs.entry(top.to_path_buf()).or_insert_with(|| git_dir_of(top));
        Some((top.to_path_buf(), git_dir.clone()?))
    }

    fn holds_git(&mut self, dir: &Path) -> bool {
        *self.probed.entry(dir.to_path_buf()).or_insert_with(|| dir.join(".git").exists())
    }
}

/// The git directory of the work tree whose top is `top`: its `.git` directory, or the
/// one its `.git` file names.
fn git_dir_of(top: &Path) -> Option<PathBuf> {
    let dot_git = top.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    linked_git_dir(top, &dot_git)
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

/// The rules of the work tree whose top is `top`, read again only when a file they came
/// from has changed since they were last read.
fn rules_of(top: &Path, git_dir: &Path) -> Arc<WorkTreeRules> {
    let kept = READ.lock().unwrap_or_else(PoisonError::into_inner).get(top).cloned();
    if let Some(kept) = kept.filter(|kept| kept.git_dir == git_dir && kept.is_current()) {
        return kept.rules;
    }
    let read_at = SystemTime::now();
    let mut sources = Vec::new();
    let rules = Arc::new(read_rules(top, git_dir, &mut sources));
    let mut read = READ.lock().unwrap_or_else(PoisonError::into_inner);
    if sources.iter().all(|(_, stamp)| stamp.settled(read_at)) {
        let git_dir = git_dir.to_path_buf();
        let kept = Read { git_dir, sources: sources.into(), rules: rules.clone() };
        read.insert(top.to_path_buf(), kept);
    } else {
        read.remove(top);
    }
    rules
}

/// The rules of each work tree read so far, by its top.
static READ: LazyLock<Mutex<HashMap<PathBuf, Read>>> = LazyLock::new(Mutex::default);

/// Rules as read, with the state each file they came from was in.
#[derive(Clone)]
struct Read {
    git_dir: PathBuf,
    sources: Arc<[(PathBuf, Stamp)]>,
    rules: Arc<WorkTreeRules>,
}

impl Read {
    fn is_current(&self) -> bool {
        self.sources.iter().all(|(path, stamp)| Stamp::of(path) == *stamp)
    }
}

/// What the metadata of a file says about its content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stamp {
    Absent,
    Present {
        len: u64,
        modified: SystemTime,
    },
    /// The metadata could not be read, or carries no modification time: never current.
    Unknown,
}

/// How long after a change a file's state can be trusted: a file changed again within
/// the resolution of its modification time keeps its length and time, so state read
/// sooner than this after a change is not kept.
const SETTLED_AFTER: Duration = Duration::from_secs(3);

impl Stamp {
    fn of(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(metadata) => match metadata.modified() {
                Ok(modified) => Self::Present { len: metadata.len(), modified },
                Err(_) => Self::Unknown,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Absent,
            Err(_) => Self::Unknown,
        }
    }

    /// Whether the state, taken just before `read_at`, can stand for what was read.
    fn settled(&self, read_at: SystemTime) -> bool {
        match self {
            Self::Absent => true,
            Self::Present { modified, .. } => {
                read_at.duration_since(*modified).is_ok_and(|age| age >= SETTLED_AFTER)
            }
            Self::Unknown => false,
        }
    }
}

/// Reads the rules, recording in `sources` the state of every file they come from, each
/// taken before the file is read: a change made while reading is seen next time.
fn read_rules(top: &Path, git_dir: &Path, sources: &mut Vec<(PathBuf, Stamp)>) -> WorkTreeRules {
    let mut stamp = |path: &Path| sources.push((path.to_path_buf(), Stamp::of(path)));
    stamp(&git_dir.join("commondir"));
    let common = common_dir(git_dir);
    let exclude = common.join("info").join("exclude");
    let gitignore = top.join(".gitignore");
    stamp(&exclude);
    stamp(&gitignore);
    // Git lets a `.gitignore` override `info/exclude`, and in the rules a later line wins
    // over an earlier one, so `info/exclude` goes first.
    let mut lines = ignore_file_lines(&exclude);
    lines.extend(ignore_file_lines(&gitignore));
    let worktrees = linked_worktrees(&common, top, &mut stamp);
    WorkTreeRules::new(top, lines.iter().map(String::as_str), &worktrees)
}

/// The lines of an ignore file, without a leading byte order mark; none when the file
/// cannot be read. Git reads an ignore file as bytes, so one saved in a legacy code page
/// still counts: only a line that is not UTF-8, which cannot name a path the analyzer
/// sees, is skipped, with a warning.
fn ignore_file_lines(file: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(file) else { return Vec::new() };
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf".as_slice()).unwrap_or(&bytes);
    let mut skipped = 0usize;
    let lines = bytes
        .split(|&byte| byte == b'\n')
        .filter_map(|line| {
            let line = line.strip_suffix(b"\r".as_slice()).unwrap_or(line);
            let decoded = std::str::from_utf8(line).ok().map(str::to_owned);
            skipped += usize::from(decoded.is_none());
            decoded
        })
        .collect();
    if skipped > 0 {
        tracing::warn!(?file, skipped, "ignore file lines that are not UTF-8 were skipped");
    }
    lines
}

/// The repository's linked worktrees below `top`, relative to it. Each is listed in
/// `worktrees/<id>/gitdir` of the common directory by the path of its `.git` file.
/// `stamp` records each file the list depends on, before it is read.
fn linked_worktrees(common: &Path, top: &Path, stamp: &mut impl FnMut(&Path)) -> Vec<PathBuf> {
    let listing = common.join("worktrees");
    // Adding or removing a worktree adds or removes an entry here.
    stamp(&listing);
    let Ok(entries) = std::fs::read_dir(&listing) else { return Vec::new() };
    let Ok(top) = stdx::path_exclusion::resolve(top) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let admin = entry.path();
            let gitdir = admin.join("gitdir");
            stamp(&gitdir);
            let text = std::fs::read_to_string(&gitdir).ok()?;
            // Absolute as git writes it by default; a relative path is taken from `admin`.
            let dot_git = admin.join(text.trim());
            // A worktree removed without `git worktree prune` stays listed; nothing of it
            // is left to walk.
            stamp(&dot_git);
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
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use stdx::path_exclusion::{GitIgnored, WorkTreeRules};

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

    /// Makes `relative` below `top` a linked worktree of `top`'s repository, laid out
    /// the way `git worktree add` leaves it: the worktree's `.git` file names its git
    /// directory, and the repository lists the worktree there.
    fn add_worktree(top: &Path, relative: &str) -> PathBuf {
        let worktree = top.join(relative);
        let admin = top.join(".git/worktrees").join(relative.replace('/', "-"));
        write(&admin.join("commondir"), "../..\n");
        write(&admin.join("gitdir"), &format!("{}\n", worktree.join(".git").display()));
        write(&worktree.join(".git"), &format!("gitdir: {}\n", admin.display()));
        worktree
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

    fn hash_of(exclusions: &ExcludedPaths) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        exclusions.hash(&mut hasher);
        hasher.finish()
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
    fn a_gitignore_line_overrides_info_exclude() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        write(&top.join(".git/info/exclude"), "/build/\n");
        write(&top.join(".gitignore"), "!/build/\n");
        configuration(top);
        configuration(&top.join("build/copy"));

        let project = Project::new(top).unwrap();
        assert_eq!(scanned(&project, top), [MODULE.to_owned(), format!("build/copy/{MODULE}")]);
    }

    #[test]
    fn an_ignore_file_in_a_legacy_code_page_still_counts() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        fs::create_dir_all(top.join(".git")).unwrap();
        // "Временные" in Windows-1251, as a comment and as a pattern of its own.
        const TEMPORARY: &[u8] = b"\xc2\xf0\xe5\xec\xe5\xed\xed\xfb\xe5";
        let gitignore = [
            b"# ".as_slice(),
            TEMPORARY,
            b"\r\n".as_slice(),
            TEMPORARY,
            b"/\r\n.claude/\r\n".as_slice(),
        ]
        .concat();
        fs::write(top.join(".gitignore"), gitignore).unwrap();
        configuration(top);
        configuration(&top.join(".claude/worktrees/copy"));

        let project = Project::new(top).unwrap();
        assert_eq!(scanned(&project, top), [MODULE]);
    }

    #[test]
    fn ignore_file_lines_drop_a_byte_order_mark_and_only_the_lines_that_are_not_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".gitignore");
        fs::write(&file, b"\xef\xbb\xbf.claude/\r\n\xc2\xf0/\r\nbuild/").unwrap();

        assert_eq!(super::ignore_file_lines(&file), [".claude/", "build/"]);
        assert!(super::ignore_file_lines(&dir.path().join("absent")).is_empty());
    }

    #[test]
    fn a_linked_worktree_inside_the_project_is_not_a_source() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        configuration(top);
        let worktree = add_worktree(top, "nested/copy");
        configuration(&worktree);

        let project = Project::new(top).unwrap();
        assert_eq!(scanned(&project, top), [MODULE]);
        assert!(project.is_source_excluded(&worktree.join(MODULE)));

        // Opened on its own, the worktree is the project, and nothing of it is left out.
        let opened = Project::new(&worktree).unwrap();
        assert_eq!(scanned(&opened, &worktree), [MODULE]);
    }

    #[test]
    fn an_unrelated_worktree_or_a_comment_leaves_the_exclusions_equal() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        fs::create_dir_all(top.join(".git")).unwrap();
        write(&top.join(".gitignore"), ".claude/\n");
        configuration(&top.join("src/cf"));
        let before = Project::new(top).unwrap();
        assert_eq!(before.source_roots(), [top.join("src/cf")]);

        add_worktree(top, ".claude/worktrees/one");
        add_worktree(top, "tools/two");
        write(&top.join(".gitignore"), "# local tool state\n\n.claude/\n");
        let after = Project::new(top).unwrap();
        assert_eq!(after.source_exclusions(), before.source_exclusions());
        assert_eq!(hash_of(after.source_exclusions()), hash_of(before.source_exclusions()));

        let inside = add_worktree(top, "src/cf/three");
        configuration(&inside);
        let changed = Project::new(top).unwrap();
        assert_ne!(changed.source_exclusions(), before.source_exclusions());
        assert_eq!(scanned(&changed, top), [format!("src/cf/{MODULE}")]);
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

    /// Sets the modification time of `file` back by `age`, past the window in which a
    /// change could go unseen, so that its state is kept with the rules read from it.
    fn settle(file: &Path, age: Duration) {
        let file = fs::File::options().write(true).open(file).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    /// Whether `rules`, the rules of the work tree at `top`, leave out the directory `dir`.
    fn leaves_out(rules: &Arc<WorkTreeRules>, top: &Path, dir: &str) -> bool {
        let mut ignored = GitIgnored::default();
        ignored.add_root(top, Some((rules.clone(), Path::new(""))));
        let excluded = ExcludedPaths::default().with_git_ignored(ignored);
        excluded.is_excluded(&top.join(dir).join(MODULE))
    }

    #[test]
    fn rules_are_read_again_only_after_a_file_they_came_from_changes() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        let git_dir = top.join(".git");
        let gitignore = top.join(".gitignore");
        let exclude = git_dir.join("info/exclude");
        write(&gitignore, ".claude/\n");
        write(&exclude, "/tools/\n");
        settle(&gitignore, Duration::from_secs(3600));
        settle(&exclude, Duration::from_secs(3600));

        let first = super::rules_of(top, &git_dir);
        let again = super::rules_of(top, &git_dir);
        assert!(Arc::ptr_eq(&first, &again), "unchanged files were read again");
        assert!(leaves_out(&first, top, ".claude"));
        assert!(!leaves_out(&first, top, "build"));

        write(&gitignore, "build/\n");
        settle(&gitignore, Duration::from_secs(1800));
        let edited = super::rules_of(top, &git_dir);
        assert!(!Arc::ptr_eq(&first, &edited), "an edited .gitignore was not read again");
        assert!(leaves_out(&edited, top, "build"));
        assert!(!leaves_out(&edited, top, ".claude"));
        assert!(Arc::ptr_eq(&edited, &super::rules_of(top, &git_dir)));

        write(&exclude, "/tools/\n/vendor/\n");
        settle(&exclude, Duration::from_secs(1800));
        let excluded = super::rules_of(top, &git_dir);
        assert!(!Arc::ptr_eq(&edited, &excluded), "an edited info/exclude was not read again");
        assert!(leaves_out(&excluded, top, "vendor"));

        let worktree = add_worktree(top, "nested/copy");
        let listed = super::rules_of(top, &git_dir);
        assert!(!Arc::ptr_eq(&excluded, &listed), "a worktree added was not seen");
        assert!(leaves_out(&listed, top, "nested/copy"));
        fs::remove_dir_all(&worktree).unwrap();
        let removed = super::rules_of(top, &git_dir);
        assert!(!leaves_out(&removed, top, "nested/copy"), "a removed worktree is still listed");

        // A file changed moments ago may change again within its time stamp's resolution
        // without a trace, so what was read from it is not kept.
        write(&gitignore, "build/\ngen/\n");
        let fresh = super::rules_of(top, &git_dir);
        assert!(leaves_out(&fresh, top, "gen"));
        assert!(!Arc::ptr_eq(&fresh, &super::rules_of(top, &git_dir)));
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
