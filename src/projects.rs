use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ProjectsConfig;

const MAX_PROJECTS: usize = 2_000;
const MAX_SCANNED_DIRECTORIES: usize = 10_000;
const MAX_SCANNED_ENTRIES: usize = 50_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Project {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) group: String,
    pub(crate) group_order: usize,
}

struct Candidate {
    path: PathBuf,
    group: String,
    group_order: usize,
}

struct RootContext<'a> {
    label: &'a str,
    order: usize,
    max_depth: usize,
}

#[derive(Default)]
struct ScanBudget {
    directories: usize,
    entries: usize,
}

pub(crate) struct Discovery {
    pub(crate) projects: Vec<Project>,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn discover(config: &ProjectsConfig) -> Discovery {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    let mut warnings = Vec::new();
    let mut budget = ScanBudget::default();
    for (group_order, root) in config.roots.iter().enumerate() {
        if !root.is_dir() {
            warnings.push(format!(
                "project root is not a directory: {}",
                root.display()
            ));
            continue;
        }
        let label = root_label(root);
        let context = RootContext {
            label: &label,
            order: group_order,
            max_depth: config.max_depth,
        };
        if let Err(error) = scan(root, &context, 0, &mut paths, &mut seen, &mut budget) {
            warnings.push(format!("could not scan {}: {error}", root.display()));
        }
        if paths.len() >= MAX_PROJECTS
            || budget.directories >= MAX_SCANNED_DIRECTORIES
            || budget.entries >= MAX_SCANNED_ENTRIES
        {
            break;
        }
    }
    if budget.directories >= MAX_SCANNED_DIRECTORIES {
        warnings.push(format!(
            "project scan stopped after {MAX_SCANNED_DIRECTORIES} directories"
        ));
    }
    if budget.entries >= MAX_SCANNED_ENTRIES {
        warnings.push(format!(
            "project scan stopped after {MAX_SCANNED_ENTRIES} filesystem entries"
        ));
    }
    if paths.len() >= MAX_PROJECTS {
        warnings.push(format!(
            "project scan stopped after {MAX_PROJECTS} projects"
        ));
    }

    let mut projects: Vec<_> = paths
        .into_iter()
        .map(|path| Project {
            name: path
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.path.display().to_string()),
            path: path.path,
            group: path.group,
            group_order: path.group_order,
        })
        .collect();
    projects.sort_by_cached_key(|project| {
        (
            project.group_order,
            project.name.to_lowercase(),
            project.path.to_string_lossy().to_lowercase(),
        )
    });
    Discovery { projects, warnings }
}

fn scan(
    directory: &Path,
    context: &RootContext<'_>,
    depth: usize,
    projects: &mut Vec<Candidate>,
    seen: &mut HashSet<PathBuf>,
    budget: &mut ScanBudget,
) -> Result<(), Box<dyn Error>> {
    if budget.directories >= MAX_SCANNED_DIRECTORIES || budget.entries >= MAX_SCANNED_ENTRIES {
        return Ok(());
    }
    budget.directories += 1;
    if is_project(directory) {
        if let Ok(directory) = directory.canonicalize()
            && seen.insert(directory.clone())
        {
            projects.push(Candidate {
                path: directory,
                group: context.label.to_owned(),
                group_order: context.order,
            });
        }
        return Ok(());
    }
    if depth >= context.max_depth || projects.len() >= MAX_PROJECTS {
        return Ok(());
    }

    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if depth == 0 => return Err(error.into()),
        Err(_) => return Ok(()),
    };
    let mut children = Vec::new();
    for entry in entries {
        if budget.entries >= MAX_SCANNED_ENTRIES {
            break;
        }
        budget.entries += 1;
        let Ok(entry) = entry else {
            continue;
        };
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && !ignored(entry.file_name().to_string_lossy().as_ref())
        {
            children.push(entry.path());
        }
    }
    children.sort_by_cached_key(|path| path.to_string_lossy().to_lowercase());
    for child in children {
        scan(&child, context, depth + 1, projects, seen, budget)?;
        if projects.len() >= MAX_PROJECTS
            || budget.directories >= MAX_SCANNED_DIRECTORIES
            || budget.entries >= MAX_SCANNED_ENTRIES
        {
            break;
        }
    }
    Ok(())
}

fn is_project(directory: &Path) -> bool {
    directory.join(".git").exists()
}

fn root_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

fn ignored(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target")
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("boomux-projects-{nonce}"));
            fs::create_dir_all(&path).expect("test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discovers_git_repositories_and_stops_at_projects() {
        let root = TestDirectory::new();
        fs::create_dir_all(root.0.join("team/alpha/.git")).expect("alpha repository");
        fs::create_dir_all(root.0.join("team/alpha/nested/.git")).expect("nested repository");
        fs::create_dir_all(root.0.join("beta/.git")).expect("beta repository");
        fs::create_dir_all(root.0.join("plain")).expect("plain directory");
        let config = ProjectsConfig {
            roots: vec![root.0.clone()],
            max_depth: 3,
        };

        let projects = discover(&config).projects;

        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "alpha");
        assert_eq!(projects[1].name, "beta");
    }

    #[test]
    fn search_depth_limits_discovery() {
        let root = TestDirectory::new();
        fs::create_dir_all(root.0.join("group/deep/project/.git")).expect("repository");
        let config = ProjectsConfig {
            roots: vec![root.0.clone()],
            max_depth: 2,
        };

        assert!(discover(&config).projects.is_empty());
    }

    #[test]
    fn recognizes_git_worktree_marker_files() {
        let root = TestDirectory::new();
        let project = root.0.join("worktree");
        fs::create_dir_all(&project).expect("worktree directory");
        fs::write(project.join(".git"), "gitdir: /tmp/example").expect("worktree marker");
        let config = ProjectsConfig {
            roots: vec![root.0.clone()],
            max_depth: 1,
        };

        let projects = discover(&config).projects;

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "worktree");
    }

    #[test]
    fn missing_roots_warn_without_hiding_valid_projects() {
        let root = TestDirectory::new();
        fs::create_dir_all(root.0.join("project/.git")).expect("repository");
        let config = ProjectsConfig {
            roots: vec![root.0.join("missing"), root.0.clone()],
            max_depth: 1,
        };

        let discovery = discover(&config);

        assert_eq!(discovery.projects.len(), 1);
        assert_eq!(discovery.warnings.len(), 1);
    }

    #[test]
    fn assigns_projects_to_configured_root_groups() {
        let root = TestDirectory::new();
        let projects_root = root.0.join("Projects");
        let work_root = root.0.join("Work");
        fs::create_dir_all(projects_root.join("personal/.git")).expect("personal repository");
        fs::create_dir_all(work_root.join("service/.git")).expect("work repository");
        let config = ProjectsConfig {
            roots: vec![projects_root, work_root],
            max_depth: 1,
        };

        let discovery = discover(&config);

        assert_eq!(discovery.projects[0].group, "Projects");
        assert_eq!(discovery.projects[0].name, "personal");
        assert_eq!(discovery.projects[1].group, "Work");
        assert_eq!(discovery.projects[1].name, "service");
    }
}
