use std::path::{Path, PathBuf};

/// Resolves a working directory to a stable project key: the nearest
/// enclosing git root (a linked worktree resolves to its main repository),
/// or the directory itself, with forward slashes.
pub fn resolve(start: &Path) -> String {
    let abs =
        std::fs::canonicalize(start).or_else(|_| std::path::absolute(start)).unwrap_or_else(|_| start.to_path_buf());
    let root = abs.ancestors().find_map(git_root).unwrap_or(abs);
    clean(&root)
}

/// The project root for `dir` if it contains a `.git` entry.
fn git_root(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return Some(dir.to_path_buf());
    }
    if !dot_git.is_file() {
        return None;
    }
    // A `.git` file points at the real git dir. A linked worktree's git dir
    // has a `commondir` file naming the shared repository; a submodule's
    // does not, so a submodule is its own project.
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = dir.join(pointer.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim());
    let Ok(common) = std::fs::read_to_string(gitdir.join("commondir")) else {
        return Some(dir.to_path_buf());
    };
    let common = gitdir.join(common.trim());
    let common = std::fs::canonicalize(&common).unwrap_or(common);
    let name = common.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let bare = std::fs::read_to_string(common.join("config"))
        .is_ok_and(|config| config.lines().any(|l| l.split_whitespace().collect::<String>() == "bare=true"));
    if name == ".git" {
        // `<repo>/.git`: the repository's own working tree.
        common.parent().map(Path::to_path_buf)
    } else if bare {
        // The `<proj>/.bare` convention belongs to `<proj>`; a bare repository
        // named like `repo.git` is its own project.
        if name.starts_with('.') {
            common.parent().map(Path::to_path_buf)
        } else {
            Some(common)
        }
    } else {
        // e.g. a worktree of a submodule, whose git dir lives inside the
        // outer repository's `.git/modules`: the worktree is the project.
        Some(dir.to_path_buf())
    }
}

/// Spellings of the working-tree root around `dir` other than the project
/// key, for making tool paths relative: the canonical root of this working
/// tree (for a linked worktree, the worktree rather than the main
/// repository), and that root as the host spelled it (unresolved junction,
/// mapped drive or UNC path).
pub fn root_aliases(dir: &Path) -> Vec<String> {
    let canonical =
        std::fs::canonicalize(dir).or_else(|_| std::path::absolute(dir)).unwrap_or_else(|_| dir.to_path_buf());
    let work_root = canonical.ancestors().find(|d| d.join(".git").exists()).unwrap_or(&canonical).to_path_buf();
    let depth = canonical.strip_prefix(&work_root).map(|rest| rest.components().count()).unwrap_or(0);
    let mut raw_root = crate::capture::normalize(&dir.to_string_lossy()).trim_end_matches('/').to_string();
    for _ in 0..depth {
        match raw_root.rfind('/') {
            Some(i) => raw_root.truncate(i),
            None => break,
        }
    }
    let mut aliases = vec![clean(&work_root)];
    if !raw_root.is_empty() && !aliases.iter().any(|a| a.eq_ignore_ascii_case(&raw_root)) {
        aliases.push(raw_root);
    }
    aliases
}

fn clean(path: &Path) -> String {
    let s = path.to_string_lossy();
    let s = match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s.strip_prefix(r"\\?\").unwrap_or(&s).to_string(),
    };
    let mut s = s.replace('\\', "/");
    if s.len() > 1 && s.ends_with('/') && !s.ends_with(":/") {
        s.pop();
    }
    // Drive letters come back in either case depending on who built the path.
    if s.as_bytes().get(1) == Some(&b':') {
        s[..1].make_ascii_uppercase();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{clean, resolve, root_aliases};
    use std::fs;
    use std::path::Path;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("csm-project-{name}-{}", std::process::id()))
    }

    #[test]
    fn finds_git_root_from_subdirectory() {
        let base = temp("nested");
        let nested = base.join("repo").join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(base.join("repo").join(".git")).unwrap();

        let from_nested = resolve(&nested);
        let from_root = resolve(&base.join("repo"));
        assert_eq!(from_nested, from_root);
        assert!(from_root.ends_with("/repo"), "{from_root}");
        assert!(!from_root.contains('\\'));

        fs::remove_dir_all(&base).unwrap();
    }

    /// Lays out a linked worktree the way `git worktree add` does.
    fn worktree(common: &Path, wt: &Path) {
        let gitdir = common.join("worktrees").join("wt");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        fs::create_dir_all(wt.join("src")).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    }

    fn bare_config(common: &Path) {
        fs::write(common.join("config"), "[core]\n\trepositoryformatversion = 0\n\tbare = true\n").unwrap();
    }

    #[test]
    fn submodule_worktree_is_its_own_project() {
        let base = temp("subwt");
        let modules = base.join("outer").join(".git").join("modules").join("lib");
        worktree(&modules, &base.join("libwt"));
        fs::write(modules.join("config"), "[core]\n\tbare = false\n").unwrap();
        assert!(resolve(&base.join("libwt")).ends_with("/libwt"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn root_aliases_name_the_working_tree_root() {
        let base = temp("aliases");
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("pkg").join("src")).unwrap();
        let aliases = root_aliases(&repo.join("pkg").join("src"));
        assert_eq!(aliases[0], resolve(&repo));
        assert!(aliases.iter().all(|a| !a.ends_with("/src")), "{aliases:?}");

        let main = base.join("main");
        worktree(&main.join(".git"), &base.join("wt"));
        fs::create_dir_all(base.join("wt").join("pkg")).unwrap();
        let aliases = root_aliases(&base.join("wt").join("pkg"));
        assert!(aliases[0].ends_with("/wt"), "{aliases:?}");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn linked_worktree_resolves_to_main_repo() {
        let base = temp("worktree");
        let main = base.join("main");
        worktree(&main.join(".git"), &base.join("wt"));
        assert_eq!(resolve(&base.join("wt").join("src")), resolve(&main));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn bare_repo_worktrees_share_a_project() {
        let base = temp("bare");
        let proj = base.join("proj");
        worktree(&proj.join(".bare"), &proj.join("main"));
        bare_config(&proj.join(".bare"));
        let key = resolve(&proj.join("main").join("src"));
        assert!(key.ends_with("/proj"), "{key}");

        let named = base.join("repo.git");
        worktree(&named, &base.join("feature"));
        bare_config(&named);
        assert!(resolve(&base.join("feature")).ends_with("/repo.git"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn submodule_is_its_own_project() {
        let base = temp("submodule");
        let sub = base.join("outer").join("vendor").join("lib");
        fs::create_dir_all(base.join("outer").join(".git").join("modules").join("lib")).unwrap();
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(".git"), "gitdir: ../../.git/modules/lib\n").unwrap();

        assert!(resolve(&sub).ends_with("/outer/vendor/lib"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cleans_separators_and_drive_case() {
        assert_eq!(clean(Path::new(r"\\?\C:\Users\me\repo")), "C:/Users/me/repo");
        assert_eq!(clean(Path::new(r"\\?\UNC\srv\share\repo")), "//srv/share/repo");
        assert_eq!(clean(Path::new(r"c:\work\app\")), "C:/work/app");
        assert_eq!(clean(Path::new("/home/me/repo/")), "/home/me/repo");
        assert_eq!(clean(Path::new("/")), "/");
    }
}
