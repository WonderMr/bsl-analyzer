//! Directories a user has taken out of the project, and the one test every walker,
//! watcher and loader asks about them.
//!
//! The list is a hard exclusion: nothing declared inside it — a source root, an
//! extension, an opened document — brings any part of it back. That is what separates
//! it from the soft holes a cache punches in a tree, where a root declared inside the
//! hole wins for itself.
//!
//! Every exclusion is held under all the spellings a path inside it can arrive under,
//! because the callers do not agree on one: a walk yields paths spelled after the root
//! it started from, a watcher reports the path it armed, and the file system resolves
//! both to a third. Matching is by whole components, never by string prefix, so
//! `.tmp2` is not inside `.tmp`.
//!
//! Besides literal directories the set may carry glob patterns, matched against a
//! path relative to one base directory (the directory of the project config). A path
//! is excluded when it, or any directory above it below the base, matches: that is
//! how files no single directory can describe — a generated listing next to every
//! template — are taken out under the same hard rule.
//!
//! The set may also carry what git leaves out below the source roots ([`GitIgnored`]):
//! the directories the ignore rules of each root's work tree name, and the repository's
//! linked worktrees. Those apply below a root only, never to the root itself.

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// A set of excluded directories.
///
/// Equality and hashing look only at the declared spellings: the canonical ones are
/// derived from whatever the file system says at construction time, and a directory
/// created later must not make an unchanged configuration look changed.
#[derive(Debug, Clone, Default)]
pub struct ExcludedPaths {
    entries: Vec<Excluded>,
    /// Boxed so a set without patterns stays as small as it was: the set is embedded by
    /// value in many structures, enums among them.
    patterns: Option<Box<Patterns>>,
    /// Shared by every copy of the set; `None` when no root has rules.
    git: Option<Arc<GitIgnored>>,
}

/// What git leaves out below the project's source roots: the directories the ignore
/// rules of each root's work tree name, and the repository's linked worktrees.
///
/// Only directories strictly below a root are matched, against the rules of the
/// innermost root that holds them. A root that git ignores, or one inside a directory
/// the rules of an enclosing root leave out, therefore keeps everything below it: being
/// declared or found as a root is the stronger statement. Files are never matched one
/// by one: a 1C dump keeps parts of its objects (`*.bin`, text templates) out of git
/// while they stay on disk and in the configuration, so only whole directories go.
///
/// Equality looks only at what can change the files of a root: the patterns that can
/// match below it and the worktrees it loses. A comment in an ignore file, a pattern
/// anchored outside every root, or a worktree outside every root or ignored already,
/// leaves two sets equal, so consumers that compare sets do not re-scope for it.
#[derive(Debug, Clone, Default)]
pub struct GitIgnored {
    roots: Vec<GitRoot>,
    /// Every spelling of every root, by name: the innermost root of a path is found in one
    /// pass over the path's names, however many roots there are.
    index: RootIndex,
    /// Whether every spelling of a root reaches the roots nested in it under spellings of
    /// their own, so that a path a walk reaches without passing a symlink finds the same
    /// root, at the same place, under the spelling it arrives in as under its resolved
    /// one. Decided when the set is attached.
    spelled_alike: bool,
    /// Tells this set's answers apart from other sets' in [`LAST_ASKED`]: assigned when
    /// the set is attached, shared by its clones, and no part of equality.
    id: u64,
}

#[derive(Debug, Clone)]
struct GitRoot {
    declared: PathBuf,
    /// The components of the declared spelling, and of the resolved one when that
    /// differs.
    spellings: Vec<Vec<String>>,
    /// `None` when nothing below the root is left out.
    rules: Option<RootRules>,
}

/// What the work tree of one root leaves out below it.
#[derive(Debug, Clone)]
struct RootRules {
    /// The patterns that can match a directory below the root.
    patterns: Arc<RootPatterns>,
    /// The root's path relative to the top of the work tree, `/`-separated.
    prefix: String,
    /// The linked worktrees strictly below the root that the patterns do not leave out
    /// already, relative to the top, `/`-separated.
    worktrees: Vec<String>,
}

/// The ignore rules of one git work tree, matched against the directories below it.
#[derive(Debug)]
pub struct WorkTreeRules {
    top: PathBuf,
    /// The patterns in effect, in order.
    patterns: Vec<Pattern>,
    /// All of them, for whether a root lies in a directory they leave out.
    matcher: ignore::gitignore::Gitignore,
    /// Every linked worktree of the repository below the top, relative to it,
    /// `/`-separated.
    worktrees: Vec<String>,
    /// The patterns that can match below a root, compiled once for every root they
    /// agree for: the extension roots of one repository usually keep the same ones.
    below: Mutex<Vec<Arc<RootPatterns>>>,
}

#[derive(Debug)]
struct Pattern {
    line: String,
    /// For a pattern that matches only from the top of the work tree, each of its names
    /// compiled on its own; `None` for one that matches at any depth, or whose names
    /// cannot be told apart safely, which is then kept for every root.
    anchored: Option<Vec<globset::GlobMatcher>>,
}

/// The patterns of a work tree that can match below some root, in their order.
#[derive(Debug)]
struct RootPatterns {
    /// Their positions among the work tree's patterns.
    kept: Vec<usize>,
    /// What equality compares, since the compiled matcher cannot be compared.
    lines: Vec<String>,
    matcher: ignore::gitignore::Gitignore,
}

/// The spellings of the roots as a tree of names.
#[derive(Debug, Clone)]
struct RootIndex {
    /// Whether names are compared ignoring case, as the file system compares them.
    ignore_case: bool,
    /// The first node stands for the empty path.
    nodes: Vec<IndexNode>,
}

#[derive(Debug, Clone, Default)]
struct IndexNode {
    /// The names one step further, as [`RootIndex::key`] spells them, in order, each with
    /// its node.
    children: Vec<(Box<str>, usize)>,
    /// The first root added whose spelling ends here.
    root: Option<usize>,
}

/// Glob patterns and the base they are relative to.
#[derive(Debug, Clone)]
struct Patterns {
    declared_base: PathBuf,
    /// Every prefix a matched path can arrive under, each with the base-relative path
    /// it stands for: the base under its declared and resolved spellings, and each walk
    /// root re-spelled by [`ExcludedPaths::respelled_under`].
    anchors: Vec<(PathBuf, PathBuf)>,
    globs: Arc<CompiledGlobs>,
}

#[derive(Debug)]
struct CompiledGlobs {
    sources: Vec<String>,
    set: globset::GlobSet,
}

/// A pattern that cannot be used, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPattern {
    pub pattern: String,
    pub reason: String,
}

impl std::fmt::Display for InvalidPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pattern {:?} {}", self.pattern, self.reason)
    }
}

impl std::error::Error for InvalidPattern {}

#[derive(Debug, Clone)]
struct Excluded {
    declared: PathBuf,
    spellings: Vec<PathBuf>,
}

impl ExcludedPaths {
    /// Builds the set from absolute paths.
    ///
    /// Each path is normalised lexically, so `a/../.tmp` names `.tmp` and cannot be used
    /// to slip past the rule, and resolved through the file system on a best-effort
    /// basis: a directory that does not exist yet is excluded under its declared
    /// spelling and under its nearest existing ancestor's resolved spelling with the
    /// rest appended — so an exclusion declared through a symlink still matches the
    /// resolved paths walks and loaders produce before the directory is created.
    pub fn new<I, P>(paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut entries: Vec<Excluded> = Vec::new();
        for path in paths {
            let declared = normalize_lexically(path.as_ref());
            if entries.iter().any(|entry| entry.declared == declared) {
                continue;
            }
            let mut spellings = vec![declared.clone()];
            if let Some(resolved) = resolve_as_far_as_it_goes(&declared) {
                push_unique(&mut spellings, resolved);
            }
            entries.push(Excluded { declared, spellings });
        }
        Self { entries, patterns: None, git: None }
    }

    /// Adds what git leaves out below the source roots. Roots without rules add nothing,
    /// so the set stays equal to one built without them.
    pub fn with_git_ignored(mut self, mut ignored: GitIgnored) -> Self {
        self.git = if ignored.has_rules() {
            ignored.id = NEXT_GIT_IGNORED_ID.fetch_add(1, Ordering::Relaxed);
            ignored.spelled_alike = ignored.spells_nested_roots_alike();
            Some(Arc::new(ignored))
        } else {
            None
        };
        self
    }

    /// Sets the glob patterns, matched against paths relative to the absolute `base`; a
    /// non-empty list replaces the patterns set before, since one set has one base.
    ///
    /// The syntax is `globset`'s with `/` as the separator: `*` and `?` stay within one
    /// path component, `**` crosses any number of them. A pattern without a `/` matches
    /// a name at any depth (`*.tmp` is `**/*.tmp`); `.` and empty segments are dropped. An
    /// empty pattern, an absolute one, one with a `..` segment or naming the base itself,
    /// a negation (`!x`, which a hard exclusion cannot honour) and one the glob parser
    /// rejects are refused rather than dropped: a dropped exclusion silently puts back
    /// the tree it was meant to hide. On Windows matching ignores case, like the rest of
    /// this set. An empty list leaves the set exactly as it was.
    pub fn with_patterns<I, S>(mut self, base: &Path, patterns: I) -> Result<Self, InvalidPattern>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut sources = Vec::new();
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            let invalid = |reason: String| InvalidPattern { pattern: pattern.to_owned(), reason };
            let trimmed = pattern.trim();
            if trimmed.is_empty() {
                return Err(invalid("must not be empty".to_owned()));
            }
            if trimmed.starts_with('!') {
                return Err(invalid(
                    "is a negation; negated patterns are not supported".to_owned(),
                ));
            }
            let normalized = trimmed.replace('\\', "/");
            let path = Path::new(&normalized);
            // A drive-relative Windows value (`C:tmp`) is neither absolute nor rooted.
            let prefixed = path.components().any(|c| matches!(c, Component::Prefix(_)));
            if normalized.starts_with('/') || path.is_absolute() || prefixed {
                return Err(invalid("must be relative to the config file".to_owned()));
            }
            let normalized = normalized.trim_end_matches('/');
            // Decided before `.` segments are dropped: `./temp` names the base's own `temp`.
            let has_separator = normalized.contains('/');
            // Matching sees only the names below the base, so `.` and empty segments are
            // dropped and `..` — which could only leave the base — is refused.
            let segments: Vec<&str> =
                normalized.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
            if segments.is_empty() || segments.contains(&"..") {
                return Err(invalid(
                    "must name a path inside the config directory, without `..`".to_owned(),
                ));
            }
            let normalized = segments.join("/");
            let anchored = if has_separator { normalized } else { format!("**/{normalized}") };
            let glob = globset::GlobBuilder::new(&anchored)
                .literal_separator(true)
                .case_insensitive(cfg!(windows))
                .backslash_escape(false)
                .build()
                .map_err(|error| invalid(format!("is not a valid glob: {}", error.kind())))?;
            builder.add(glob);
            sources.push(pattern.to_owned());
        }
        if sources.is_empty() {
            return Ok(self);
        }
        let set = builder.build().map_err(|error| InvalidPattern {
            pattern: sources.join(", "),
            reason: format!("cannot be compiled: {}", error.kind()),
        })?;
        let declared_base = normalize_lexically(base);
        let mut anchors = vec![(declared_base.clone(), PathBuf::new())];
        if let Some(resolved) = resolve_as_far_as_it_goes(&declared_base) {
            push_anchor(&mut anchors, resolved, PathBuf::new());
        }
        let globs = Arc::new(CompiledGlobs { sources, set });
        self.patterns = Some(Box::new(Patterns { declared_base, anchors, globs }));
        Ok(self)
    }

    /// Whether nothing is excluded: no directory, no pattern and nothing git leaves out.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.patterns.is_none() && self.git.is_none()
    }

    /// The patterns as they were declared.
    pub fn patterns(&self) -> impl Iterator<Item = &str> + '_ {
        self.patterns.iter().flat_map(|patterns| patterns.globs.sources.iter().map(String::as_str))
    }

    /// Whether a file a walk reached, inside directories it did not prune, is taken out
    /// by a pattern. Directories are pruned by [`Self::prunes_walked_dir`], so only the
    /// patterns are asked here; without patterns this costs nothing.
    ///
    /// The file is asked about under its walked spelling and under its parent's resolved
    /// one, taken from `resolved` (or resolved once and kept there), so a directory
    /// symlink does not carry a matched file back in under a name no pattern carries.
    pub fn excludes_walked_file(&self, path: &Path, resolved: &mut ResolvedDirs) -> bool {
        let Some(patterns) = &self.patterns else { return false };
        if patterns.matches_any_spelling(path) {
            return true;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        let canonical = match resolved.0.get(parent) {
            Some(parent) => parent.join(name),
            None => {
                let canonical_parent =
                    resolve_as_far_as_it_goes(parent).unwrap_or_else(|| parent.to_path_buf());
                let canonical = canonical_parent.join(name);
                resolved.0.insert(parent.to_path_buf(), canonical_parent);
                canonical
            }
        };
        patterns.matches_any_spelling(&canonical)
    }

    /// The excluded directories as they were declared (after lexical normalisation).
    pub fn declared(&self) -> impl Iterator<Item = &Path> + '_ {
        self.entries.iter().map(|entry| entry.declared.as_path())
    }

    /// A copy that also recognises each exclusion under the spelling a walk started at
    /// one of `roots` produces.
    ///
    /// A root reached through a symlink or a `..` yields entries that match neither the
    /// declared nor the resolved spelling of an exclusion inside it. Re-spelling the
    /// exclusion once per root costs one resolution per root; resolving every walked
    /// entry instead would cost one per file.
    pub fn respelled_under(&self, roots: &[PathBuf]) -> Self {
        let roots: Vec<(PathBuf, PathBuf)> = roots
            .iter()
            .map(|root| {
                let canonical = root
                    .canonicalize()
                    .map(simplify_verbatim)
                    .unwrap_or_else(|_| normalize_lexically(root));
                (root.clone(), canonical)
            })
            .collect();
        let entries = self
            .entries
            .iter()
            .map(|entry| {
                let mut spellings = entry.spellings.clone();
                for (declared_root, canonical_root) in &roots {
                    for spelling in &entry.spellings {
                        if let Ok(relative) = spelling.strip_prefix(canonical_root) {
                            push_unique(&mut spellings, declared_root.join(relative));
                        }
                    }
                }
                Excluded { declared: entry.declared.clone(), spellings }
            })
            .collect();
        let patterns =
            self.patterns.as_ref().map(|patterns| Box::new(patterns.respelled_under(&roots)));
        Self { entries, patterns, git: self.git.clone() }
    }

    /// Whether `path` is an excluded directory or lies anywhere beneath one.
    ///
    /// A directory git leaves out answers only for what lies beneath it, since the
    /// question does not say whether `path` is a directory; a walk that reaches one is
    /// pruned there by [`Self::prunes_walked_dir`].
    pub fn is_excluded(&self, path: &Path) -> bool {
        if self.is_empty() {
            return false;
        }
        self.is_listed(path) || self.git.as_ref().is_some_and(|git| git.leaves_out(path, false))
    }

    /// [`Self::is_excluded`] by the literal directories and the patterns alone.
    fn is_listed(&self, path: &Path) -> bool {
        if self.entries.is_empty() && self.patterns.is_none() {
            return false;
        }
        if self.any_spelling(|spelling| is_within(path, spelling)) {
            return true;
        }
        if self.patterns.as_ref().is_some_and(|patterns| patterns.matches_any_spelling(path)) {
            return true;
        }
        has_dot_components(path) && {
            let normalized = normalize_lexically(path);
            self.any_spelling(|spelling| is_within(&normalized, spelling))
        }
    }

    /// [`Self::is_excluded`] that also asks under the spelling the file system resolves
    /// `path` to. For a single decision about a path that may be, or pass through, a
    /// symlink into an excluded directory — not for the per-entry test of a walk, which
    /// would pay a resolution per entry. A path that does not exist yet — a document
    /// opened before it is first saved — is resolved through its nearest existing
    /// ancestor, so a symlink alias still leads into the exclusion.
    pub fn is_excluded_resolved(&self, path: &Path) -> bool {
        self.is_excluded(path)
            || (self.resolves_aliases()
                && resolve_as_far_as_it_goes(path)
                    .is_some_and(|resolved| self.is_excluded(&resolved)))
    }

    /// Whether a decision must also ask under the spelling the file system resolves a
    /// path to. Literal directories and patterns are hard exclusions that a symlink alias
    /// must not lead around. Git's rules are asked under the spelling a path arrives in,
    /// so a set that holds nothing else costs no resolution.
    pub fn resolves_aliases(&self) -> bool {
        !self.entries.is_empty() || self.patterns.is_some()
    }

    /// Whether a walk must not enter the directory at `path` it has just reached, or
    /// take the symlink at `path`, whatever it points to.
    ///
    /// Asks about the walked spelling and about the resolved one, so that a symlink
    /// aliasing an excluded directory or file — or one of their ancestors — does not
    /// lead the walk back in under a name no exclusion carries. The resolved spelling is derived from
    /// the parent's, recorded in `resolved`, so only the walk's start and symlinks cost
    /// a resolution; a walk that follows links must pass every directory it enters.
    ///
    /// A set that holds only git's rules keeps no resolved spelling for a directory a
    /// walk reaches under a root's spelling without passing a symlink: git's answer is
    /// the same under both spellings there, so the walk pays neither a resolution nor a
    /// record per directory. A symlink, and everything a walk reaches through one, is
    /// still asked about under the spelling it resolves to.
    pub fn prunes_walked_dir(
        &self,
        path: &Path,
        is_symlink: bool,
        resolved: &mut ResolvedDirs,
    ) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.prunes(path, is_symlink) {
            return true;
        }
        let from_parent = || {
            let parent = resolved.0.get(path.parent()?)?;
            Some(parent.join(path.file_name()?))
        };
        let canonical = match (is_symlink, from_parent()) {
            (false, Some(canonical)) => canonical,
            (false, None) if self.git_answers_alike(path) => return false,
            _ => path.canonicalize().map(simplify_verbatim).unwrap_or_else(|_| path.to_path_buf()),
        };
        let excluded = canonical.as_path() != path && self.prunes(&canonical, is_symlink);
        resolved.0.insert(path.to_path_buf(), canonical);
        excluded
    }

    /// Whether git's rules, the only thing this set holds, answer for the resolved
    /// spelling of a directory reached under `path` with no symlink on the way what they
    /// answer for `path`: `path` lies under a spelling of a root, and every root reaches
    /// the roots nested in it under spellings of their own.
    fn git_answers_alike(&self, path: &Path) -> bool {
        let Some(git) = self.git.as_deref() else { return false };
        !self.resolves_aliases() && git.spelled_alike && git.index.innermost(path).is_some()
    }

    /// Whether the entry at `path` lies in an excluded directory or, unless it is a
    /// symlink, is a directory git leaves out: git is asked once per spelling.
    fn prunes(&self, path: &Path, is_symlink: bool) -> bool {
        self.is_listed(path)
            || self.git.as_ref().is_some_and(|git| git.leaves_out(path, !is_symlink))
    }

    /// [`Self::is_excluded_resolved`] for many paths in a row: the resolved spelling of
    /// each parent directory is taken once and kept in `resolved`, and only a path that
    /// is itself a symlink is resolved on its own. For bulk decisions over files that
    /// may have been reached through a symlink alias of an excluded directory.
    pub fn is_excluded_resolving(&self, path: &Path, resolved: &mut ResolvedDirs) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.is_excluded(path) {
            return true;
        }
        if !self.resolves_aliases() {
            return false;
        }
        let canonical = if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        {
            path.canonicalize().ok().map(simplify_verbatim)
        } else {
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => {
                    let parent = resolved.0.entry(parent.to_path_buf()).or_insert_with(|| {
                        resolve_as_far_as_it_goes(parent).unwrap_or_else(|| parent.to_path_buf())
                    });
                    Some(parent.join(name))
                }
                _ => None,
            }
        };
        canonical.is_some_and(|canonical| self.is_excluded(&canonical))
    }

    /// Whether some excluded directory lies strictly beneath `dir`.
    ///
    /// The question a watcher asks before arming `dir` recursively: a recursive watch on
    /// such a directory would cover the exclusion too, including one that does not
    /// exist yet and is only created later. Patterns and git's rules do not count: any
    /// directory may hold a match, so they are applied to each path a watch reports instead.
    pub fn has_exclusion_below(&self, dir: &Path) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let below = |dir: &Path| {
            self.any_spelling(|spelling| is_within(spelling, dir) && !same_path(spelling, dir))
        };
        below(dir) || (has_dot_components(dir) && below(&normalize_lexically(dir)))
    }

    /// [`Self::has_exclusion_below`] that also asks under the spelling the file system
    /// resolves `dir` to: a symlink to an ancestor of an exclusion has the exclusion
    /// below it just as much, though no lexical comparison can tell.
    pub fn has_exclusion_below_resolved(&self, dir: &Path) -> bool {
        self.has_exclusion_below(dir)
            || (!self.entries.is_empty()
                && resolve_as_far_as_it_goes(dir)
                    .is_some_and(|resolved| self.has_exclusion_below(&resolved)))
    }

    /// The heap this set owns: the entries and every spelling of each.
    pub fn heap_bytes(&self) -> usize {
        let patterns = self.patterns.as_ref().map_or(0, |patterns| {
            std::mem::size_of::<Patterns>()
                + patterns.declared_base.capacity()
                + crate::heap::vec_bytes::<(PathBuf, PathBuf)>(patterns.anchors.capacity())
                + patterns
                    .anchors
                    .iter()
                    .map(|(prefix, relative)| prefix.capacity() + relative.capacity())
                    .sum::<usize>()
        });
        patterns
            + crate::heap::vec_bytes::<Excluded>(self.entries.capacity())
            + self
                .entries
                .iter()
                .map(|entry| {
                    entry.declared.capacity()
                        + crate::heap::vec_bytes::<PathBuf>(entry.spellings.capacity())
                        + entry.spellings.iter().map(PathBuf::capacity).sum::<usize>()
                })
                .sum::<usize>()
    }

    fn any_spelling(&self, mut test: impl FnMut(&Path) -> bool) -> bool {
        self.entries.iter().any(|entry| entry.spellings.iter().any(|spelling| test(spelling)))
    }
}

/// The resolved spelling of each directory one walk has entered, for
/// [`ExcludedPaths::prunes_walked_dir`]. One per walk: entries are keyed by the walk's
/// own spelling of each directory.
#[derive(Debug, Default)]
pub struct ResolvedDirs(std::collections::HashMap<PathBuf, PathBuf>);

impl Patterns {
    fn respelled_under(&self, roots: &[(PathBuf, PathBuf)]) -> Self {
        let mut anchors = self.anchors.clone();
        for (declared_root, canonical_root) in roots {
            for (prefix, relative) in &self.anchors {
                // A root inside the base: a path spelled after the root stands for the
                // root's place relative to the base, under either spelling of the root.
                if let Some(below) = strip_within(canonical_root, prefix) {
                    push_anchor(&mut anchors, declared_root.clone(), relative.join(below));
                }
                if let Some(below) = strip_within(declared_root, prefix) {
                    push_anchor(&mut anchors, canonical_root.clone(), relative.join(below));
                }
                // The base inside a root: the base re-spelled after the root.
                if relative.as_os_str().is_empty() {
                    if let Some(below) = strip_within(prefix, canonical_root) {
                        push_anchor(&mut anchors, declared_root.join(below), PathBuf::new());
                    }
                }
            }
        }
        Self { declared_base: self.declared_base.clone(), anchors, globs: self.globs.clone() }
    }

    fn matches_any_spelling(&self, path: &Path) -> bool {
        if has_dot_components(path) {
            self.matches(&normalize_lexically(path))
        } else {
            self.matches(path)
        }
    }

    fn matches(&self, path: &Path) -> bool {
        self.anchors.iter().any(|(prefix, relative)| {
            strip_within(path, prefix).is_some_and(|below| self.matches_relative(relative, &below))
        })
    }

    /// Whether the base-relative path, or a directory above it, matches a pattern.
    fn matches_relative(&self, relative: &Path, below: &Path) -> bool {
        let mut candidate = String::new();
        for component in relative.components().chain(below.components()) {
            let Component::Normal(name) = component else { continue };
            if !candidate.is_empty() {
                candidate.push('/');
            }
            candidate.push_str(&name.to_string_lossy());
            if self.globs.set.is_match(&candidate) {
                return true;
            }
        }
        false
    }
}

/// Adds an anchor unless the same pair is already there. One prefix may stand for
/// several base-relative paths — a root aliased onto the base — and each is matched.
fn push_anchor(anchors: &mut Vec<(PathBuf, PathBuf)>, prefix: PathBuf, relative: PathBuf) {
    if !anchors
        .iter()
        .any(|(existing, existing_relative)| *existing == prefix && *existing_relative == relative)
    {
        anchors.push((prefix, relative));
    }
}

/// `path` with `prefix` taken off, compared by whole components the way [`is_within`]
/// compares them; `None` when `path` is not at or under `prefix`.
fn strip_within(path: &Path, prefix: &Path) -> Option<PathBuf> {
    if !is_within(path, prefix) {
        return None;
    }
    Some(path.components().skip(prefix.components().count()).collect())
}

impl GitIgnored {
    /// Adds a source root. `tree` is the rules of the git work tree that holds the root,
    /// with the root's path relative to the top of that work tree; `None` for a root
    /// outside every work tree, which still decides for the paths inside it.
    pub fn add_root(&mut self, root: &Path, tree: Option<(Arc<WorkTreeRules>, &Path)>) {
        let declared = normalize_lexically(root);
        let mut spellings = vec![component_names(&declared)];
        if let Ok(resolved) = resolve(&declared) {
            let spelling = component_names(&resolved);
            if !spellings.contains(&spelling) {
                spellings.push(spelling);
            }
        }
        let added = self.roots.len();
        for spelling in &spellings {
            self.index.insert(spelling, added);
        }
        let rules = tree.and_then(|(tree, relative)| RootRules::new(&tree, relative));
        self.roots.push(GitRoot { declared, spellings, rules });
    }

    fn has_rules(&self) -> bool {
        self.roots.iter().any(|root| root.rules.is_some())
    }

    /// Whether every spelling of each root reaches each root nested in it under one of
    /// that root's own spellings. A root declared through a link while a root inside it is
    /// declared under the resolved spelling is the case that fails: there a walked path
    /// and its resolved spelling can have different innermost roots.
    fn spells_nested_roots_alike(&self) -> bool {
        self.roots.iter().all(|outer| {
            let Some(outer_resolved) = outer.spellings.last() else { return true };
            self.roots.iter().all(|inner| {
                let Some(inner_resolved) = inner.spellings.last() else { return true };
                let Some(tail) = self.index.names_below(inner_resolved, outer_resolved) else {
                    return true;
                };
                outer.spellings.iter().all(|spelling| {
                    let nested: Vec<&String> = spelling.iter().chain(tail).collect();
                    inner.spellings.iter().any(|other| self.index.same_names(other, &nested))
                })
            })
        })
    }

    /// Whether `path` lies in a directory below its innermost root that the root's rules
    /// leave out; with `itself`, also whether `path` is such a directory.
    fn leaves_out(&self, path: &Path, itself: bool) -> bool {
        if has_dot_components(path) {
            return self.leaves_out_normalized(&normalize_lexically(path), itself);
        }
        self.leaves_out_normalized(path, itself)
    }

    fn leaves_out_normalized(&self, path: &Path, itself: bool) -> bool {
        let Some((index, depth)) = self.index.innermost(path) else { return false };
        let Some(rules) = &self.roots[index].rules else { return false };
        // The directory whose answer this is: `path` itself, or the one holding it.
        let judged = if itself { Some(path) } else { path.parent() };
        let Some(judged) = judged.filter(|judged| judged.components().count() > depth) else {
            return false;
        };
        LAST_ASKED.with(|last| {
            let mut last = last.borrow_mut();
            let mut known_kept = 0;
            let asked = last.as_ref().filter(|asked| asked.is_about(self.id, index, depth));
            if let Some(asked) = asked {
                if asked.judged.as_os_str() == judged.as_os_str() {
                    return asked.answer;
                }
                // Every directory down to the last one asked about is kept, so of the
                // directories the two share, none needs asking again.
                if !asked.answer {
                    let shared = shared_names(Path::new(&asked.judged), judged);
                    known_kept = shared.saturating_sub(depth);
                }
            }
            let answer = rules.leaves_out_below(judged.components().skip(depth), known_kept);
            let judged = judged.as_os_str();
            match last.as_mut() {
                Some(asked) => asked.remember(self.id, index, depth, judged, answer),
                None => *last = Some(Asked::new(self.id, index, depth, judged, answer)),
            }
            answer
        })
    }
}

static NEXT_GIT_IGNORED_ID: AtomicU64 = AtomicU64::new(1);

/// The last directory one thread asked a [`GitIgnored`] about, with the answer for it and
/// every directory above it below its root.
struct Asked {
    id: u64,
    root: usize,
    /// The number of names of the root's spelling the directory was found under.
    depth: usize,
    judged: OsString,
    answer: bool,
}

impl Asked {
    fn new(id: u64, root: usize, depth: usize, judged: &OsStr, answer: bool) -> Self {
        Self { id, root, depth, judged: judged.to_owned(), answer }
    }

    fn is_about(&self, id: u64, root: usize, depth: usize) -> bool {
        self.id == id && self.root == root && self.depth == depth
    }

    /// Keeps another answer in place of this one, in the same buffer.
    fn remember(&mut self, id: u64, root: usize, depth: usize, judged: &OsStr, answer: bool) {
        self.id = id;
        self.root = root;
        self.depth = depth;
        self.judged.clear();
        self.judged.push(judged);
        self.answer = answer;
    }
}

thread_local! {
    /// Walks, watchers and loaders ask about every entry of one directory in a row, and
    /// for all of them git's answer is the directory's; a walk then goes on into a
    /// directory below the last one. Kept here, the answer spares matching the directory
    /// once per entry, and its ancestors once per directory below them.
    static LAST_ASKED: RefCell<Option<Asked>> = const { RefCell::new(None) };
}

/// How many leading names two paths share, spelled byte for byte alike.
fn shared_names(left: &Path, right: &Path) -> usize {
    left.components().zip(right.components()).take_while(|(left, right)| left == right).count()
}

impl Default for RootIndex {
    fn default() -> Self {
        Self::new(cfg!(windows))
    }
}

impl RootIndex {
    fn new(ignore_case: bool) -> Self {
        Self { ignore_case, nodes: vec![IndexNode::default()] }
    }

    /// Adds a spelling of `root`. A node another root's spelling already ends at keeps
    /// that root, the one added first.
    fn insert(&mut self, spelling: &[String], root: usize) {
        let mut node = 0;
        for name in spelling {
            let key = self.key(name);
            let children = &self.nodes[node].children;
            let found = children.binary_search_by(|(existing, _)| str::cmp(existing, &key));
            node = match found {
                Ok(at) => children[at].1,
                Err(at) => {
                    let child = self.nodes.len();
                    self.nodes.push(IndexNode::default());
                    self.nodes[node].children.insert(at, (key.into_boxed_str(), child));
                    child
                }
            };
        }
        self.nodes[node].root.get_or_insert(root);
    }

    /// The innermost root `path` lies at or below, with the number of names of the
    /// spelling that holds it: one step per name of `path`, whatever the number of roots.
    fn innermost(&self, path: &Path) -> Option<(usize, usize)> {
        let mut node = self.nodes.first()?;
        let mut innermost = node.root.map(|root| (root, 0));
        for (at, component) in path.components().enumerate() {
            let name = component.as_os_str().to_string_lossy();
            let found = node.children.binary_search_by(|(key, _)| self.order(key, &name));
            let Ok(found) = found else { break };
            node = &self.nodes[node.children[found].1];
            if let Some(root) = node.root {
                innermost = Some((root, at + 1));
            }
        }
        innermost
    }

    /// The names of `path` past `prefix` when `path` lies strictly below it, both given by
    /// name and compared as the index compares them.
    fn names_below<'a>(&self, path: &'a [String], prefix: &[String]) -> Option<&'a [String]> {
        let tail = path.get(prefix.len()..).filter(|tail| !tail.is_empty())?;
        let within = path.iter().zip(prefix).all(|(name, other)| self.same_name(name, other));
        within.then_some(tail)
    }

    /// Whether two spellings, given by name, are the same as the index compares them.
    fn same_names(&self, left: &[String], right: &[&String]) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|(name, other)| self.same_name(name, other))
    }

    fn same_name(&self, left: &str, right: &str) -> bool {
        left == right || (self.ignore_case && folded(left).eq(folded(right)))
    }

    /// `name` as the index keeps it: folded to lower case where case is ignored.
    fn key(&self, name: &str) -> String {
        if self.ignore_case {
            folded(name).collect()
        } else {
            name.to_owned()
        }
    }

    /// How `key` orders against the key of `name`, without spelling that key out.
    fn order(&self, key: &str, name: &str) -> std::cmp::Ordering {
        if !self.ignore_case {
            key.cmp(name)
        } else if name.is_ascii() {
            key.bytes().cmp(name.bytes().map(|byte| byte.to_ascii_lowercase()))
        } else {
            key.chars().cmp(folded(name))
        }
    }
}

/// `name` folded to lower case, character by character.
fn folded(name: &str) -> impl Iterator<Item = char> + '_ {
    name.chars().flat_map(char::to_lowercase)
}

impl GitRoot {
    /// What equality and hashing look at: the declared spelling, the root's place in its
    /// work tree, the patterns that can match below it and the worktrees it loses.
    fn identity(&self) -> (&Path, Option<&str>, &[String], &[String]) {
        let Some(rules) = &self.rules else {
            return (self.declared.as_path(), None, &[], &[]);
        };
        (
            self.declared.as_path(),
            Some(rules.prefix.as_str()),
            &rules.patterns.lines,
            &rules.worktrees,
        )
    }
}

impl PartialEq for GitIgnored {
    fn eq(&self, other: &Self) -> bool {
        self.roots.iter().map(GitRoot::identity).eq(other.roots.iter().map(GitRoot::identity))
    }
}

impl Eq for GitIgnored {}

impl Hash for GitIgnored {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for root in &self.roots {
            root.identity().hash(state);
        }
    }
}

impl RootRules {
    /// `None` when the patterns leave out the root itself, since a root is still a root,
    /// and when nothing below the root can be left out.
    fn new(tree: &WorkTreeRules, relative: &Path) -> Option<Self> {
        let mut prefix = String::new();
        let mut names = Vec::new();
        for component in relative.components() {
            let Component::Normal(name) = component else { continue };
            push_name(&mut prefix, name);
            if tree.ignores(&prefix) {
                return None;
            }
            names.push(name.to_string_lossy().into_owned());
        }
        let worktrees: Vec<String> = tree
            .worktrees
            .iter()
            .filter(|worktree| tree.adds_below(&prefix, worktree))
            .cloned()
            .collect();
        let patterns = tree.patterns_below(&names);
        if patterns.lines.is_empty() && worktrees.is_empty() {
            return None;
        }
        Some(Self { patterns, prefix, worktrees })
    }

    /// Whether the directory at `relative` (to the top, `/`-separated) is left out.
    fn leaves_out(&self, relative: &str) -> bool {
        self.patterns.matcher.matched(Path::new(relative), true).is_ignore()
            || self.worktrees.iter().any(|worktree| same_name(worktree, relative))
    }

    /// Whether a directory from just below the root down to the last of `names` (the
    /// names below the root) is left out, the first `known_kept` of them known not to be.
    fn leaves_out_below<'a>(
        &self,
        names: impl Iterator<Item = Component<'a>>,
        known_kept: usize,
    ) -> bool {
        let mut relative = self.prefix.clone();
        for (at, name) in names.enumerate() {
            push_name(&mut relative, name.as_os_str());
            if at >= known_kept && self.leaves_out(&relative) {
                return true;
            }
        }
        false
    }
}

impl WorkTreeRules {
    /// Rules of the work tree whose top is `top`: `lines` in gitignore format, relative
    /// to `top` and in the order git weighs them (a later line wins over an earlier
    /// one), and `worktrees`, directories relative to `top`, each left out whole. A line
    /// that is not a valid pattern is skipped, as git skips it.
    pub fn new<'a>(
        top: &Path,
        lines: impl IntoIterator<Item = &'a str>,
        worktrees: &[PathBuf],
    ) -> Self {
        let mut builder = gitignore_builder(top);
        let mut patterns = Vec::new();
        for line in lines {
            // As git reads a line: trailing spaces go unless escaped, and a blank line or a
            // comment is no pattern, so editing one changes nothing.
            let line = if line.ends_with("\\ ") { line } else { line.trim_end() };
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if builder.add_line(None, line).is_ok() {
                patterns.push(Pattern { line: line.to_owned(), anchored: anchored_names(line) });
            }
        }
        let matcher = builder.build().unwrap_or_else(|_| ignore::gitignore::Gitignore::empty());
        let worktrees = worktrees
            .iter()
            .map(PathBuf::as_path)
            .map(slash_separated)
            .filter(|worktree| !worktree.is_empty())
            .collect();
        Self { top: top.to_path_buf(), patterns, matcher, worktrees, below: Mutex::default() }
    }

    /// Whether the patterns leave out the directory at `relative` (to the top,
    /// `/`-separated).
    fn ignores(&self, relative: &str) -> bool {
        self.matcher.matched(Path::new(relative), true).is_ignore()
    }

    /// The patterns that can match a directory below a root whose path relative to the
    /// top has the names `prefix`. The others cannot change any answer below the root, so
    /// the root neither matches against them nor changes when they do.
    fn patterns_below(&self, prefix: &[String]) -> Arc<RootPatterns> {
        let can_match = |at: &usize| self.patterns[*at].can_match_below(prefix);
        let kept: Vec<usize> = (0..self.patterns.len()).filter(can_match).collect();
        let mut compiled = self.below.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = compiled.iter().find(|patterns| patterns.kept == kept) {
            return found.clone();
        }
        let mut builder = gitignore_builder(&self.top);
        let lines: Vec<String> = kept.iter().map(|&at| self.patterns[at].line.clone()).collect();
        for line in &lines {
            let _ = builder.add_line(None, line);
        }
        let matcher = builder.build().unwrap_or_else(|_| ignore::gitignore::Gitignore::empty());
        let patterns = Arc::new(RootPatterns { kept, lines, matcher });
        compiled.push(patterns.clone());
        patterns
    }

    /// Whether `worktree` lies strictly below `prefix` (both relative to the top,
    /// `/`-separated) and no directory from just below `prefix` down to the worktree is
    /// ignored already.
    fn adds_below(&self, prefix: &str, worktree: &str) -> bool {
        let mut names = worktree.split('/');
        for expected in prefix.split('/').filter(|expected| !expected.is_empty()) {
            if !names.next().is_some_and(|name| same_name(name, expected)) {
                return false;
            }
        }
        let mut candidate = prefix.to_owned();
        let mut below = false;
        for name in names {
            push_name(&mut candidate, OsStr::new(name));
            if self.ignores(&candidate) {
                return false;
            }
            below = true;
        }
        below
    }
}

impl Pattern {
    /// Whether the pattern can match a directory strictly below a root whose path
    /// relative to the top has the names `prefix`: one that matches from the top only
    /// cannot when it has no more names than the root, or when one of its first names does
    /// not match the root's name in that place.
    fn can_match_below(&self, prefix: &[String]) -> bool {
        let Some(names) = &self.anchored else { return true };
        names.len() > prefix.len()
            && names.iter().zip(prefix).all(|(name, root_name)| name.is_match(root_name))
    }
}

/// The names of a pattern that matches only from the top of the work tree, each compiled
/// the way the rules compile the whole pattern, so that matching them one by one against
/// one name each agrees with matching the pattern. `None` for a pattern that matches at
/// any depth, and for one this does not take apart: a `**`, an escape, a class or an
/// alternation can span names.
fn anchored_names(line: &str) -> Option<Vec<globset::GlobMatcher>> {
    let line = line.strip_prefix('!').unwrap_or(line);
    let (from_top, glob) = match line.strip_prefix('/') {
        Some(glob) => (true, glob),
        None => (false, line),
    };
    let glob = glob.strip_suffix('/').unwrap_or(glob);
    if !from_top && !glob.contains('/') {
        return None;
    }
    if glob.contains("**") || glob.contains(['\\', '[', ']', '{', '}']) {
        return None;
    }
    if glob.split('/').any(str::is_empty) {
        return None;
    }
    let compile = |name: &str| -> Option<globset::GlobMatcher> {
        let glob = globset::GlobBuilder::new(name)
            .literal_separator(true)
            .case_insensitive(cfg!(windows))
            .backslash_escape(true)
            .build()
            .ok()?;
        Some(glob.compile_matcher())
    };
    glob.split('/').map(compile).collect()
}

/// A builder for rules relative to `top`, matching names as git does on this system.
fn gitignore_builder(top: &Path) -> ignore::gitignore::GitignoreBuilder {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(top);
    // Git ignores case in its patterns where the file system does (`core.ignorecase`),
    // and so does the rest of this set.
    let _ = builder.case_insensitive(cfg!(windows));
    builder
}

/// The components of `path` as text, for [`RootIndex`].
fn component_names(path: &Path) -> Vec<String> {
    let name = |component: Component<'_>| component.as_os_str().to_string_lossy().into_owned();
    path.components().map(name).collect()
}

/// `path`'s plain names joined by `/`.
fn slash_separated(path: &Path) -> String {
    let mut out = String::new();
    for component in path.components() {
        if let Component::Normal(name) = component {
            push_name(&mut out, name);
        }
    }
    out
}

fn push_name(path: &mut String, name: &OsStr) {
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(&name.to_string_lossy());
}

/// Whether two names, or two `/`-separated relative paths, are the same, ignoring case
/// on Windows as [`is_within`] does. Names spelled alike, the common case, cost one byte
/// comparison; only a pair that differs is folded, character by character and without
/// allocating.
fn same_name(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    #[cfg(test)]
    FOLDED_COMPARISONS.with(|count| count.set(count.get() + 1));
    cfg!(windows)
        && left.chars().flat_map(char::to_lowercase).eq(right.chars().flat_map(char::to_lowercase))
}

#[cfg(test)]
thread_local! {
    /// How many name comparisons on this thread got past byte equality.
    static FOLDED_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl PartialEq for ExcludedPaths {
    fn eq(&self, other: &Self) -> bool {
        self.entries.len() == other.entries.len()
            && self.entries.iter().zip(&other.entries).all(|(a, b)| a.declared == b.declared)
            && match (&self.patterns, &other.patterns) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    a.declared_base == b.declared_base && a.globs.sources == b.globs.sources
                }
                _ => false,
            }
            && self.git == other.git
    }
}

impl Eq for ExcludedPaths {}

impl Hash for ExcludedPaths {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.entries.len().hash(state);
        for entry in &self.entries {
            entry.declared.hash(state);
        }
        if let Some(patterns) = &self.patterns {
            patterns.declared_base.hash(state);
            patterns.globs.sources.hash(state);
        }
        if let Some(git) = &self.git {
            git.hash(state);
        }
    }
}

/// Resolves `.` and `..` without touching the file system. A `..` that would climb
/// above the root of an absolute path is dropped, the way the file system treats it.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let popped =
                    matches!(out.components().next_back(), Some(Component::Normal(_))) && out.pop();
                if !popped && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` with every link resolved, in the spelling walks and events use (no Windows
/// verbatim prefix).
pub fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(simplify_verbatim)
}

/// [`resolve`] of the nearest existing ancestor of `path`, with the part that does not
/// exist yet appended; `None` when not even the root resolves.
///
/// The appended part is normalised lexically, `..` included: nothing in it exists, so
/// nothing in it can be a symlink that `..` would have to step back out of.
pub fn resolve_as_far_as_it_goes(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(resolved) = resolve(existing) {
            let mut out = resolved;
            for component in missing.iter().rev() {
                out.push(component);
            }
            return Some(normalize_lexically(&out));
        }
        missing.push(existing.components().next_back()?.as_os_str().to_os_string());
        existing = existing.parent()?;
    }
}

fn has_dot_components(path: &Path) -> bool {
    path.components().any(|component| matches!(component, Component::CurDir | Component::ParentDir))
}

fn push_unique(spellings: &mut Vec<PathBuf>, spelling: PathBuf) {
    if !spellings.contains(&spelling) {
        spellings.push(spelling);
    }
}

/// `path` with a verbatim disk prefix rewritten as the ordinary spelling: `\\?\C:\x`
/// becomes `C:\x`. `canonicalize` answers with the verbatim form on Windows, and no
/// walk entry, watcher event or diff report is spelled that way, so the two sides of
/// a comparison name the same file differently. Any other prefix is returned
/// untouched — a UNC share among them, whose verbatim form names different
/// components. Purely lexical: nothing is read from the file system.
#[cfg(windows)]
pub fn simplify_verbatim(path: PathBuf) -> PathBuf {
    use std::path::Prefix;
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) => {
                let mut out = PathBuf::from(format!("{}:\\", letter as char));
                for component in components {
                    if !matches!(component, Component::RootDir) {
                        out.push(component.as_os_str());
                    }
                }
                out
            }
            _ => path,
        },
        _ => path,
    }
}

/// Nothing to simplify off Windows: the verbatim prefix is a Windows spelling.
#[cfg(not(windows))]
pub fn simplify_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// Whether `path` is `prefix` or lies beneath it, compared by whole components. On
/// Windows the file system ignores case, so the comparison does too.
fn is_within(path: &Path, prefix: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut path = path.components();
        prefix.components().all(|expected| {
            path.next().is_some_and(|actual| {
                actual.as_os_str().to_string_lossy().to_lowercase()
                    == expected.as_os_str().to_string_lossy().to_lowercase()
            })
        })
    }
    #[cfg(not(windows))]
    {
        path.starts_with(prefix)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    is_within(left, right) && left.components().count() == right.components().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute(name: &str) -> PathBuf {
        std::path::absolute(Path::new("target").join("path-exclusion-tests").join(name)).unwrap()
    }

    #[test]
    fn exclusions_are_component_scoped_and_normalized() {
        let root = absolute("component");
        let excluded = ExcludedPaths::new([root.join("a/../.tmp")]);

        assert!(excluded.is_excluded(&root.join(".tmp")));
        assert!(excluded.is_excluded(&root.join("./.tmp/child/Module.bsl")));
        assert!(!excluded.is_excluded(&root.join(".tmp2/Module.bsl")));
        assert!(!excluded.is_excluded(&root.join("allowed/Module.bsl")));
    }

    fn with_patterns(base: &Path, patterns: &[&str]) -> ExcludedPaths {
        ExcludedPaths::default().with_patterns(base, patterns).unwrap()
    }

    #[test]
    fn patterns_match_files_and_directories_relative_to_the_base() {
        let base = absolute("patterns");
        let excluded = with_patterns(&base, &["temp", ".tool/**", "**/Generated.bsl", "src/gen"]);

        for hidden in [
            "temp",
            "temp/Module.bsl",
            "src/cf/temp/deep/Module.bsl",
            ".tool/Module.bsl",
            "src/cf/Reports/R/Templates/T/Ext/Generated.bsl",
            "src/gen/Module.bsl",
            "./src/../temp/Module.bsl",
        ] {
            assert!(excluded.is_excluded(&base.join(hidden)), "{hidden} must be excluded");
            assert!(
                excluded.excludes_walked_file(&base.join(hidden), &mut ResolvedDirs::default()),
                "{hidden} (walk)"
            );
        }
        for visible in [
            "temporary/Module.bsl",
            "src/cf/Reports/R/Ext/ObjectModule.bsl",
            "src/cf/Generated.bsl.txt",
            "other/src/gen/Module.bsl",
            ".tool2/Module.bsl",
        ] {
            assert!(!excluded.is_excluded(&base.join(visible)), "{visible} must stay");
        }
        assert!(
            !excluded.is_excluded(&absolute("elsewhere").join("temp/Module.bsl")),
            "a pattern matched a path outside its base"
        );
        assert!(!excluded.is_empty());
        assert_eq!(
            excluded.patterns().collect::<Vec<_>>(),
            ["temp", ".tool/**", "**/Generated.bsl", "src/gen"]
        );
    }

    #[test]
    fn a_star_does_not_cross_a_separator_in_an_anchored_pattern() {
        let base = absolute("separator");
        let excluded = with_patterns(&base, &["src/*.bsl"]);

        assert!(excluded.is_excluded(&base.join("src/Module.bsl")));
        assert!(!excluded.is_excluded(&base.join("src/nested/Module.bsl")));
    }

    #[test]
    fn a_leading_dot_slash_anchors_the_pattern_to_the_base() {
        let base = absolute("dot-slash");
        let excluded = with_patterns(&base, &["./temp"]);

        assert!(excluded.is_excluded(&base.join("temp/Module.bsl")));
        assert!(!excluded.is_excluded(&base.join("src/temp/Module.bsl")));
    }

    #[test]
    fn dot_and_empty_segments_are_dropped_from_a_pattern() {
        let base = absolute("dot-segments");
        for pattern in ["src/./gen", "src//gen", ".//src/gen", "./src/./gen/"] {
            let excluded = with_patterns(&base, &[pattern]);
            assert!(excluded.is_excluded(&base.join("src/gen/Module.bsl")), "{pattern}");
            assert!(!excluded.is_excluded(&base.join("other/src/gen/Module.bsl")), "{pattern}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn patterns_ignore_case_on_windows() {
        let base = absolute("case");
        let excluded = with_patterns(&base, &["**/Generated.bsl"]);

        assert!(excluded.is_excluded(&base.join("Ext/GENERATED.BSL")));
    }

    #[test]
    fn patterns_follow_a_walk_root_respelled_under_another_name() {
        let base = absolute("respelled");
        let root = base.join("src/cf");
        let excluded = with_patterns(&base, &["src/cf/temp"]);
        // A root reached under another spelling: the walk yields paths under `alias`,
        // which the canonical root stands for.
        let alias = absolute("respelled-alias");
        let respelled = excluded.respelled_under(std::slice::from_ref(&root));
        assert!(respelled.is_excluded(&root.join("temp/Module.bsl")));

        let roots = [(alias.clone(), root.clone())];
        let patterns = excluded.patterns.as_ref().unwrap().respelled_under(&roots);
        assert!(patterns.matches(&alias.join("temp/Module.bsl")));
        assert!(!patterns.matches(&alias.join("kept/Module.bsl")));

        // The other direction: a root declared inside the base through a link whose
        // target lies elsewhere — files keyed by the target still match.
        let target = absolute("respelled-target");
        let roots = [(root.clone(), target.clone())];
        let patterns = excluded.patterns.as_ref().unwrap().respelled_under(&roots);
        assert!(patterns.matches(&target.join("temp/Module.bsl")));
        assert!(!patterns.matches(&target.join("kept/Module.bsl")));
    }

    #[test]
    fn unusable_patterns_are_refused_with_the_offending_value() {
        let base = absolute("invalid");
        for (pattern, reason) in [
            ("", "must not be empty"),
            ("   ", "must not be empty"),
            ("!temp", "negation"),
            ("/temp", "relative"),
            ("a[", "not a valid glob"),
            ("../shared", "inside the config directory"),
            ("src/../../x", "inside the config directory"),
            ("a/../temp", "inside the config directory"),
            (".", "inside the config directory"),
            ("./", "inside the config directory"),
        ] {
            let error = ExcludedPaths::default().with_patterns(&base, [pattern]).unwrap_err();
            assert_eq!(error.pattern, pattern);
            assert!(error.to_string().contains(reason), "{pattern:?}: {error}");
        }
        #[cfg(windows)]
        assert!(ExcludedPaths::default().with_patterns(&base, ["C:temp"]).is_err());
    }

    #[test]
    fn no_patterns_leave_the_set_unchanged() {
        let base = absolute("none");
        let plain = ExcludedPaths::new([base.join(".tmp")]);
        let with_none = plain.clone().with_patterns(&base, Vec::<String>::new()).unwrap();

        assert_eq!(plain, with_none);
        assert!(with_none.patterns().next().is_none());
        assert!(!with_none
            .excludes_walked_file(&base.join("temp/Module.bsl"), &mut ResolvedDirs::default()));
        assert!(ExcludedPaths::default().with_patterns(&base, [""; 0]).unwrap().is_empty());
        assert_ne!(plain, plain.clone().with_patterns(&base, ["temp"]).unwrap());
    }

    #[test]
    fn a_future_exclusion_already_narrows_its_existing_parent() {
        let root = absolute("future");
        let future = root.join("missing/deep/.tmp");
        let excluded = ExcludedPaths::new([future.clone()]);

        assert!(excluded.has_exclusion_below(&root));
        assert!(excluded.has_exclusion_below(&root.join("missing")));
        assert!(!excluded.has_exclusion_below(&future));
        assert!(excluded.is_excluded(&future.join("Module.bsl")));
        assert!(!excluded.is_excluded(&root.join("missing/deep/.tmp2/Module.bsl")));
    }

    #[test]
    fn declared_identity_does_not_change_when_a_path_can_be_resolved() {
        let root = absolute("identity");
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join(".tmp");
        let before = ExcludedPaths::new([path.clone()]);
        std::fs::create_dir_all(&path).unwrap();
        let after = ExcludedPaths::new([path.clone()]);

        assert_eq!(before, after);
        assert!(after.is_excluded_resolved(&path.join("Module.bsl")));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_alias_cannot_reenter_an_excluded_directory() {
        let root = absolute("symlink");
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        let excluded_dir = real.join(".tmp");
        std::fs::create_dir_all(&excluded_dir).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let excluded = ExcludedPaths::new([excluded_dir.clone()]);

        assert!(excluded.is_excluded_resolved(&alias.join(".tmp/Module.bsl")));
        assert!(excluded.is_excluded_resolved(&alias.join(".tmp/missing/../StillHidden.bsl")));
        assert!(!excluded.is_excluded_resolved(&alias.join(".tmp2/Module.bsl")));
        assert!(!excluded.is_excluded_resolved(&alias.join("allowed/missing/../StillAllowed.bsl")));

        let declared_through_alias = ExcludedPaths::new([alias.join(".tmp")]);
        assert!(declared_through_alias.is_excluded_resolved(&real.join(".tmp/Module.bsl")));
        assert!(!declared_through_alias.is_excluded_resolved(&real.join(".tmp2/Module.bsl")));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_pattern_follows_a_file_reached_through_a_directory_symlink() {
        let base = absolute("pattern-alias");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/Ext")).unwrap();
        std::fs::write(base.join("real/Ext/Template.bsl"), "").unwrap();
        std::fs::write(base.join("real/Kept.bsl"), "").unwrap();
        std::os::unix::fs::symlink(base.join("real/Ext"), base.join("alias")).unwrap();
        let excluded = with_patterns(&base, &["real/Ext/*.bsl"]);

        let mut walked = crate::fs::parallel_walk_paths(
            &[base.as_path()],
            &crate::fs::WalkConfig {
                extensions: &["bsl"],
                excludes: &[],
                excluded: Some(&excluded),
                follow_links: true,
            },
        )
        .into_vec();
        walked.sort();
        assert_eq!(walked, [base.join("real/Kept.bsl")]);

        let mut resolved = ResolvedDirs::default();
        assert!(excluded.excludes_walked_file(&base.join("alias/Template.bsl"), &mut resolved));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_root_respelled_onto_the_base_keeps_its_own_anchor() {
        let base = absolute("respelled-onto-base");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::os::unix::fs::symlink(&base, base.join("x")).unwrap();
        let excluded = with_patterns(&base, &["x/temp"])
            .respelled_under(std::slice::from_ref(&base.join("x")));

        assert!(excluded.is_excluded(&base.join("x/temp/Module.bsl")));
        assert!(excluded.is_excluded(&base.join("temp/Module.bsl")));
        assert!(!excluded.is_excluded(&base.join("kept/Module.bsl")));

        std::fs::remove_dir_all(&base).unwrap();
    }

    const MODULE: &str = "CommonModules/M/Ext/Module.bsl";

    /// A set over `roots`, relative to the work tree top `name`, with the rules
    /// `gitignore` and the linked `worktrees`.
    fn git_ignored(
        name: &str,
        gitignore: &str,
        roots: &[&str],
        worktrees: &[&str],
    ) -> (PathBuf, ExcludedPaths) {
        let top = absolute(name);
        let worktrees: Vec<PathBuf> = worktrees.iter().map(PathBuf::from).collect();
        let rules = Arc::new(WorkTreeRules::new(&top, gitignore.lines(), &worktrees));
        let mut ignored = GitIgnored::default();
        for root in roots {
            ignored.add_root(&top.join(root), Some((rules.clone(), Path::new(root))));
        }
        (top, ExcludedPaths::default().with_git_ignored(ignored))
    }

    fn hash_of(set: &ExcludedPaths) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        set.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn git_ignored_directories_below_a_root_are_left_out_but_files_are_not() {
        let (top, excluded) = git_ignored("git-ignored", ".claude/\n*.bin\n/build\n", &[""], &[]);

        assert!(excluded.is_excluded(&top.join(".claude/worktrees/copy").join(MODULE)));
        assert!(excluded.is_excluded(&top.join("build").join(MODULE)));
        assert!(!excluded.is_excluded(&top.join("src/build").join(MODULE)), "`/build` is anchored");
        assert!(!excluded.is_excluded(&top.join(MODULE)));
        assert!(
            !excluded.is_excluded(&top.join("CommonModules/M/Ext/Module.bin")),
            "a file is never matched on its own"
        );
        let mut resolved = ResolvedDirs::default();
        assert!(excluded.prunes_walked_dir(&top.join(".claude"), false, &mut resolved));
        assert!(!excluded.prunes_walked_dir(&top.join("CommonModules"), false, &mut resolved));
        assert!(!excluded.prunes_walked_dir(&top, false, &mut resolved), "the root itself stays");
        assert!(!excluded.resolves_aliases(), "git's rules alone cost no resolution");
        assert!(ExcludedPaths::new([top.join(".tmp")]).resolves_aliases());
    }

    #[test]
    fn a_later_line_wins_over_an_earlier_one() {
        let (top, kept) = git_ignored("git-order", "/build/\n!/build/\n", &[""], &[]);
        let (_, left_out) = git_ignored("git-order", "!/build/\n/build/\n", &[""], &[]);

        assert!(!kept.is_excluded(&top.join("build").join(MODULE)));
        assert!(left_out.is_excluded(&top.join("build").join(MODULE)));
    }

    #[test]
    fn a_root_inside_an_ignored_directory_keeps_everything_below_it() {
        let roots = ["", "build/cf", "work/cf"];
        let (top, excluded) = git_ignored("git-ignored-root", "build/\nwork/**\n", &roots, &[]);

        assert!(excluded.is_excluded(&top.join("build/other").join(MODULE)));
        assert!(!excluded.is_excluded(&top.join("build/cf").join(MODULE)));
        assert!(!excluded.is_excluded(&top.join("work/cf").join(MODULE)));
        let mut resolved = ResolvedDirs::default();
        assert!(!excluded.prunes_walked_dir(&top.join("build/cf"), false, &mut resolved));
    }

    #[test]
    fn a_worktree_below_a_root_is_left_out_whole() {
        let (top, excluded) = git_ignored("git-listed", "", &[""], &["nested/copy"]);

        assert!(excluded.is_excluded(&top.join("nested/copy").join(MODULE)));
        assert!(!excluded.is_excluded(&top.join("nested/other").join(MODULE)));
        assert!(!excluded.is_excluded(&top.join("nested/copy")), "only what lies below it");
    }

    #[test]
    fn a_comment_or_a_worktree_that_changes_no_root_keeps_the_set_equal() {
        let roots = ["src/cf"];
        let (_, before) = git_ignored("git-equal", ".claude/\n", &roots, &[]);
        // Outside the root and ignored, outside the root, and above the root.
        let unrelated = [".claude/worktrees/one", "tools/two", "src"];
        let (_, after) = git_ignored("git-equal", "# tools\n\n.claude/  \n", &roots, &unrelated);
        let (top, inside) = git_ignored("git-equal", ".claude/\n", &roots, &["src/cf/three"]);

        assert_eq!(before, after);
        assert_eq!(hash_of(&before), hash_of(&after));
        assert_ne!(before, inside, "a worktree a root loses changes the set");
        assert!(inside.is_excluded(&top.join("src/cf/three").join(MODULE)));

        let (_, plain) = git_ignored("git-equal-top", ".claude/\n", &[""], &[]);
        let (_, ignored) = git_ignored("git-equal-top", ".claude/\n", &[""], &[".claude/x"]);
        assert_eq!(plain, ignored, "a worktree the patterns leave out already adds nothing");
    }

    #[test]
    fn rules_that_leave_nothing_out_change_nothing() {
        let (top, excluded) = git_ignored("git-inert", "# a comment only\n", &[""], &[]);

        assert!(excluded.is_empty());
        assert_eq!(excluded, ExcludedPaths::default());
        let mut outside = GitIgnored::default();
        outside.add_root(&top, None);
        assert_eq!(ExcludedPaths::default().with_git_ignored(outside), ExcludedPaths::default());
    }

    #[test]
    fn different_patterns_make_different_sets() {
        let (_, first) = git_ignored("git-patterns", ".claude/\n", &[""], &[]);
        let (_, other) = git_ignored("git-patterns", "build/\n", &[""], &[]);

        assert_ne!(first, other);
        assert_ne!(first, ExcludedPaths::default());
    }

    #[test]
    fn a_lookup_over_many_roots_compares_equal_names_without_folding() {
        let names: Vec<String> = (0..25).map(|n| format!("src/cfe/Extension{n}")).collect();
        let roots: Vec<&str> = names.iter().map(String::as_str).collect();
        let (top, excluded) = git_ignored("git-many-roots", ".claude/\n", &roots, &[]);
        let file = top.join("src/cfe/Extension7").join(MODULE);

        FOLDED_COMPARISONS.with(|count| count.set(0));
        assert!(!excluded.is_excluded(&file));
        let folded = FOLDED_COMPARISONS.with(std::cell::Cell::get);
        // Each root that does not hold the file parts from it at one name.
        assert!(folded < roots.len(), "{folded} comparisons were folded for {} roots", roots.len());
    }

    #[test]
    fn an_answer_kept_for_a_directory_belongs_to_one_set() {
        let (top, build) = git_ignored("git-memo", "build/\n", &[""], &[]);
        let (_, tools) = git_ignored("git-memo", "tools/\n", &[""], &[]);

        assert!(build.is_excluded(&top.join("build/A.bsl")));
        assert!(!tools.is_excluded(&top.join("build/B.bsl")), "one set used another's answer");
        assert!(build.is_excluded(&top.join("build/B.bsl")));
        assert!(!build.is_excluded(&top.join("src/A.bsl")));
        assert!(build.is_excluded(&top.join("build/deeper/A.bsl")));
        assert!(tools.is_excluded(&top.join("tools/A.bsl")));
    }

    #[cfg(windows)]
    #[test]
    fn git_ignored_directories_ignore_case_on_windows() {
        let (top, excluded) = git_ignored("git-case", ".Claude/\n", &[""], &["Nested/Copy"]);

        assert!(excluded.is_excluded(&top.join(".claude/worktrees").join(MODULE)));
        assert!(excluded.is_excluded(&top.join("nested/copy").join(MODULE)));
    }

    /// An index over `roots`, `/`-separated relative paths added in order.
    fn index_of(ignore_case: bool, roots: &[&str]) -> RootIndex {
        let mut index = RootIndex::new(ignore_case);
        for (at, root) in roots.iter().enumerate() {
            index.insert(&component_names(Path::new(root)), at);
        }
        index
    }

    #[test]
    fn the_index_finds_the_innermost_of_nested_roots() {
        let roots = ["top", "top/src/cfe/A", "top/src/cfe/A/nested/B", "top/src/cfe/C"];
        let index = index_of(false, &roots);
        let innermost = |path: &str| index.innermost(Path::new(path));

        assert_eq!(innermost("top/src/cfe/A/CommonModules/M"), Some((1, 4)));
        assert_eq!(innermost("top/src/cfe/A/nested/B/Ext/Module.bsl"), Some((2, 6)));
        assert_eq!(innermost("top/src/cfe/A/nested/Other"), Some((1, 4)));
        assert_eq!(innermost("top/src/cfe/C"), Some((3, 4)));
        assert_eq!(innermost("top/src/cfe/AB/Module.bsl"), Some((0, 1)), "names, not prefixes");
        assert_eq!(innermost("top"), Some((0, 1)));
        assert_eq!(innermost("elsewhere/src/cfe/A"), None);
        assert_eq!(innermost("to"), None);
    }

    #[test]
    fn the_index_ignores_case_where_the_file_system_does() {
        let roots = ["Top/Src/РасширениеУчета", "Top/Src/Ext"];
        let ignoring = index_of(true, &roots);
        let exact = index_of(false, &roots);

        let folded = Path::new("top/SRC/расширениеучета/Module.bsl");
        assert_eq!(ignoring.innermost(folded), Some((0, 3)));
        assert_eq!(ignoring.innermost(Path::new("TOP/src/EXT")), Some((1, 3)));
        assert_eq!(exact.innermost(folded), None);
        assert_eq!(exact.innermost(Path::new("Top/Src/Ext/Module.bsl")), Some((1, 3)));
        assert_eq!(exact.innermost(Path::new("Top/Src/EXT/Module.bsl")), None);
    }

    #[test]
    fn of_two_roots_spelled_alike_the_one_added_first_holds_a_path() {
        let index = index_of(true, &["top/Cf", "top/cf"]);

        assert_eq!(index.innermost(Path::new("top/CF/Module.bsl")), Some((0, 2)));
    }

    /// `relative` (`/`-separated) below `base`, joined name by name: a verbatim Windows
    /// path takes only `\` as a separator.
    #[cfg(windows)]
    fn under(base: &Path, relative: &str) -> PathBuf {
        relative.split('/').fold(base.to_path_buf(), |path, name| path.join(name))
    }

    #[cfg(windows)]
    #[test]
    fn a_root_declared_verbatim_holds_its_paths_under_either_spelling() {
        let base = absolute("git-verbatim");
        std::fs::create_dir_all(base.join("src/cf")).unwrap();
        let verbatim = base.canonicalize().unwrap();
        assert!(verbatim.to_string_lossy().starts_with(r"\\?\"), "{}", verbatim.display());
        let plain = simplify_verbatim(verbatim.clone());
        let rules = Arc::new(WorkTreeRules::new(&verbatim, ".claude/\n".lines(), &[]));
        let mut ignored = GitIgnored::default();
        ignored.add_root(&under(&verbatim, "src/cf"), Some((rules, Path::new("src/cf"))));
        let excluded = ExcludedPaths::default().with_git_ignored(ignored);

        for top in [&verbatim, &plain] {
            let copy = under(top, "src/cf/.claude/worktrees/copy");
            assert!(excluded.is_excluded(&under(&copy, MODULE)), "{}", copy.display());
            let kept = under(top, "src/cf/CommonModules");
            assert!(!excluded.is_excluded(&under(&kept, "M/Ext/Module.bsl")), "{}", kept.display());
        }
    }

    #[test]
    fn a_root_keeps_only_the_patterns_that_can_match_below_it() {
        let rules = "/tools/\nsrc/cfe/A/gen/\nsrc/*/B/gen/\nsrc/**/deep/\n.claude/\n";
        let roots = ["src/cfe/A", "src/cfe/B", "src/cfe/C"];
        let (top, excluded) = git_ignored("git-below", rules, &roots, &[]);

        assert!(excluded.is_excluded(&top.join("src/cfe/A/gen").join(MODULE)));
        assert!(excluded.is_excluded(&top.join("src/cfe/B/gen").join(MODULE)));
        assert!(!excluded.is_excluded(&top.join("src/cfe/C/gen").join(MODULE)));
        assert!(excluded.is_excluded(&top.join("src/cfe/C/x/deep").join(MODULE)));
        assert!(excluded.is_excluded(&top.join("src/cfe/C/.claude").join(MODULE)));

        // A pattern anchored outside every root, or below a directory that is no root,
        // changes no root.
        let fewer = "src/cfe/A/gen/\nsrc/*/B/gen/\nsrc/**/deep/\n.claude/\n";
        let (_, without) = git_ignored("git-below", fewer, &roots, &[]);
        assert_eq!(without, excluded);
        assert_eq!(hash_of(&without), hash_of(&excluded));
        let more = format!("{rules}/docs/\nsrc/cfe/D/x/\n");
        assert_eq!(git_ignored("git-below", &more, &roots, &[]).1, excluded);
        let inside = format!("{rules}src/cfe/C/x/\n");
        assert_ne!(git_ignored("git-below", &inside, &roots, &[]).1, excluded);

        let (_, outside) = git_ignored("git-below-none", "/tools/\nsrc/cfe/D/\n", &roots, &[]);
        assert!(outside.is_empty(), "patterns that match below no root leave nothing to ask");
    }

    #[test]
    fn an_answer_kept_for_a_directory_does_not_hide_a_directory_below_it() {
        let (top, excluded) = git_ignored("git-memo-below", "build/\n", &[""], &[]);

        assert!(!excluded.is_excluded(&top.join("a/b/M.bsl")));
        assert!(excluded.is_excluded(&top.join("a/b/build/M.bsl")));
        assert!(!excluded.is_excluded(&top.join("a/M.bsl")));
        assert!(excluded.is_excluded(&top.join("a/b/c/build/d/M.bsl")));
        assert!(!excluded.is_excluded(&top.join("a/b/c/d/M.bsl")));
        assert!(excluded.is_excluded(&top.join("build/M.bsl")));
        let mut resolved = ResolvedDirs::default();
        assert!(excluded.prunes_walked_dir(&top.join("a/b/c/build"), false, &mut resolved));
        assert!(!excluded.prunes_walked_dir(&top.join("a/b/c/kept"), false, &mut resolved));
    }

    #[test]
    fn a_walk_under_a_root_keeps_no_resolved_spelling_for_git_alone() {
        let (top, excluded) = git_ignored("git-walk", ".claude/\n", &[""], &[]);
        let mut resolved = ResolvedDirs::default();

        assert!(!excluded.prunes_walked_dir(&top, false, &mut resolved));
        for dir in ["CommonModules", "CommonModules/M", "CommonModules/M/Ext"] {
            assert!(!excluded.prunes_walked_dir(&top.join(dir), false, &mut resolved), "{dir}");
        }
        assert!(excluded.prunes_walked_dir(&top.join(".claude"), false, &mut resolved));
        assert!(resolved.0.is_empty(), "resolved {:?}", resolved.0.keys().collect::<Vec<_>>());

        // Outside every root there is no root to answer alike: the walk resolves.
        assert!(!excluded.prunes_walked_dir(&absolute("git-walk-outside"), false, &mut resolved));
        assert_eq!(resolved.0.len(), 1);
        // A literal exclusion is asked under resolved spellings too.
        let listed = ExcludedPaths::new([top.join(".tmp")]).with_git_ignored(GitIgnored::default());
        let mut resolved = ResolvedDirs::default();
        assert!(!listed.prunes_walked_dir(&top.join("CommonModules"), false, &mut resolved));
        assert_eq!(resolved.0.len(), 1);
    }

    /// A set of git's rules over `roots`, each with its path relative to the top.
    #[cfg(unix)]
    fn git_ignored_at(rules: &Arc<WorkTreeRules>, roots: &[(PathBuf, &str)]) -> ExcludedPaths {
        let mut ignored = GitIgnored::default();
        for (root, relative) in roots {
            ignored.add_root(root, Some((rules.clone(), Path::new(relative))));
        }
        ExcludedPaths::default().with_git_ignored(ignored)
    }

    #[cfg(unix)]
    #[test]
    fn a_root_nested_under_another_spelling_keeps_a_walk_resolving() {
        let base = absolute("git-alias-roots");
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        std::fs::create_dir_all(real.join("top/inner/sub")).unwrap();
        std::os::unix::fs::symlink(&real, base.join("alias")).unwrap();
        let outer = base.join("alias/top");
        let rules = Arc::new(WorkTreeRules::new(&outer, ".claude/\n".lines(), &[]));

        // The inner root is declared under the resolved spelling only, so a walk of the
        // outer root through the link must still ask under the resolved spelling.
        let apart = [(outer.clone(), ""), (real.join("top/inner"), "inner")];
        let mut resolved = ResolvedDirs::default();
        let set = git_ignored_at(&rules, &apart);
        assert!(!set.prunes_walked_dir(&outer.join("inner/sub"), false, &mut resolved));
        assert!(!resolved.0.is_empty());

        let alike = [(outer.clone(), ""), (outer.join("inner"), "inner")];
        let mut resolved = ResolvedDirs::default();
        let set = git_ignored_at(&rules, &alike);
        assert!(!set.prunes_walked_dir(&outer.join("inner/sub"), false, &mut resolved));
        assert!(resolved.0.is_empty());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_still_leads_git_to_what_it_points_at() {
        let top = absolute("git-symlink");
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(top.join(".claude/worktrees/copy/CommonModules")).unwrap();
        std::fs::create_dir_all(top.join("build/sub")).unwrap();
        std::os::unix::fs::symlink(top.join(".claude/worktrees/copy"), top.join("copy")).unwrap();
        std::os::unix::fs::symlink(top.join("build"), top.join("output")).unwrap();
        let rules = Arc::new(WorkTreeRules::new(&top, ".claude/\nbuild/\n".lines(), &[]));
        let excluded = git_ignored_at(&rules, &[(top.clone(), "")]);
        let mut resolved = ResolvedDirs::default();

        assert!(excluded.prunes_walked_dir(&top.join("copy"), true, &mut resolved));
        assert!(!excluded.prunes_walked_dir(&top.join("output"), true, &mut resolved));
        assert!(excluded.prunes_walked_dir(&top.join("output/sub"), false, &mut resolved));

        std::fs::remove_dir_all(&top).unwrap();
    }
}
