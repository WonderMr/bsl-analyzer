//! The file system as the metadata loaders may read it: the real one, less the
//! directories the user took out of the project.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bsl_conventions::{DirTree, EntryKind, RealFs, TreeEntry};
use stdx::path_exclusion::ExcludedPaths;

/// [`RealFs`] that never lists, probes or reports anything inside an excluded
/// directory. An excluded directory looks absent; its parent lists without it.
///
/// A directory is also asked about under its resolved spelling, once per listing, so a
/// symlink alias of an excluded directory is just as absent.
#[derive(Clone, Copy)]
pub struct ScopedFs<'a> {
    excluded: &'a ExcludedPaths,
}

impl<'a> ScopedFs<'a> {
    pub fn new(excluded: &'a ExcludedPaths) -> Self {
        Self { excluded }
    }

    /// Whether `path` is excluded under its own spelling or the one it resolves to.
    pub fn is_excluded(&self, path: &Path) -> bool {
        self.excluded.is_excluded_resolved(path)
    }

    /// `std::fs::read_dir` over what may be read: an excluded directory reads as absent,
    /// and excluded children are left out.
    pub fn read_dir(
        &self,
        dir: &Path,
    ) -> std::io::Result<impl Iterator<Item = std::io::Result<std::fs::DirEntry>> + 'a> {
        if self.is_excluded(dir) {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        let resolved = self.resolved_dir(dir);
        let excluded = self.excluded;
        let asks = excluded.may_exclude_an_entry_of(dir);
        Ok(std::fs::read_dir(dir)?.filter(move |entry| match entry {
            Ok(entry) if asks => {
                !child_excluded(excluded, &entry.path(), resolved.as_deref(), &entry.file_name())
            }
            _ => true,
        }))
    }

    /// The resolved spelling of `dir` when it differs from the declared one — the second
    /// name its children can be excluded under.
    fn resolved_dir(&self, dir: &Path) -> Option<PathBuf> {
        if !self.excluded.resolves_aliases() {
            return None;
        }
        stdx::path_exclusion::resolve(dir).ok().filter(|resolved| resolved != dir)
    }
}

fn child_excluded(
    excluded: &ExcludedPaths,
    path: &Path,
    resolved_parent: Option<&Path>,
    name: &std::ffi::OsStr,
) -> bool {
    if excluded.is_empty() {
        return false;
    }
    excluded.is_excluded(path)
        || resolved_parent.is_some_and(|parent| excluded.is_excluded(&parent.join(name)))
        || (excluded.resolves_aliases()
            && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
            && excluded.is_excluded_resolved(path))
}

impl DirTree for ScopedFs<'_> {
    fn kind_of(&self, path: &Path) -> Option<EntryKind> {
        if self.is_excluded(path) {
            return None;
        }
        RealFs.kind_of(path)
    }

    fn entries(&self, dir: &Path) -> Vec<TreeEntry> {
        if self.is_excluded(dir) {
            return Vec::new();
        }
        let entries = RealFs.entries(dir);
        if !self.excluded.may_exclude_an_entry_of(dir) {
            return entries;
        }
        let resolved = self.resolved_dir(dir);
        entries
            .into_iter()
            .filter(|entry| {
                let name = entry.path.file_name().unwrap_or_default();
                !child_excluded(self.excluded, &entry.path, resolved.as_deref(), name)
            })
            .collect()
    }

    fn child_names(&self, dir: &Path) -> Vec<OsString> {
        if self.is_excluded(dir) {
            return Vec::new();
        }
        let names = RealFs.child_names(dir);
        if !self.excluded.may_exclude_an_entry_of(dir) {
            return names;
        }
        let resolved = self.resolved_dir(dir);
        names
            .into_iter()
            .filter(|name| {
                !child_excluded(self.excluded, &dir.join(name), resolved.as_deref(), name)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_metadata_is_absent_from_every_filesystem_projection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hidden = root.join("Catalogs/Hidden");
        let visible = root.join("Catalogs/Visible");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::create_dir_all(&visible).unwrap();
        std::fs::write(hidden.join("Hidden.xml"), "secret").unwrap();
        std::fs::write(visible.join("Visible.xml"), "public").unwrap();
        let excluded = ExcludedPaths::new([hidden.clone()]);
        let fs = ScopedFs::new(&excluded);

        let names = fs.child_names(&root.join("Catalogs"));
        assert!(!names.iter().any(|name| name == "Hidden"));
        assert!(names.iter().any(|name| name == "Visible"));
        assert!(fs.entries(&hidden).is_empty());
        assert!(fs.kind_of(&hidden.join("Hidden.xml")).is_none());
        assert_eq!(
            fs.read_dir(&hidden).err().expect("excluded directory must be absent").kind(),
            std::io::ErrorKind::NotFound
        );

        let visible_files: Vec<_> =
            fs.read_dir(&visible).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(visible_files, [OsString::from("Visible.xml")]);
    }

    #[test]
    fn a_directory_git_leaves_out_lists_as_empty() {
        use stdx::path_exclusion::{GitIgnored, WorkTreeRules};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let copy = root.join(".claude/worktrees/copy");
        std::fs::create_dir_all(copy.join("Catalogs")).unwrap();
        std::fs::create_dir_all(root.join("Catalogs/Visible")).unwrap();
        std::fs::write(root.join("Catalogs/Visible.xml"), "public").unwrap();
        let rules = std::sync::Arc::new(WorkTreeRules::new(root, ".claude/\n".lines(), &[]));
        let mut ignored = GitIgnored::default();
        ignored.add_root(root, Some((rules, Path::new(""))));
        let excluded = ExcludedPaths::default().with_git_ignored(ignored);
        let fs = ScopedFs::new(&excluded);

        let names = fs.child_names(root);
        assert!(names.iter().any(|name| name == ".claude"), "{names:?}");
        assert!(names.iter().any(|name| name == "Catalogs"), "{names:?}");
        let catalogs = fs.child_names(&root.join("Catalogs"));
        assert_eq!(catalogs.len(), 2, "{catalogs:?}");
        assert!(fs.child_names(&root.join(".claude")).is_empty());
        assert!(fs.entries(&root.join(".claude")).is_empty());
        assert!(fs.read_dir(&root.join(".claude")).unwrap().next().is_none());
        assert!(fs.kind_of(&copy.join("Catalogs")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_metadata_symlink_cannot_alias_an_excluded_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let hidden = real.join("Hidden");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("Object.xml"), "secret").unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let excluded = ExcludedPaths::new([hidden]);
        let fs = ScopedFs::new(&excluded);

        assert!(fs.child_names(&alias).iter().all(|name| name != "Hidden"));
        assert!(fs.kind_of(&alias.join("Hidden/Object.xml")).is_none());
    }
}
