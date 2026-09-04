//! Extension file discovery (pi v0.84.3 loader discovery): global
//! `~/.pillar/extensions` first, then project-local
//! `.pillar/extensions`; `*.luau` files plus `*/index.luau`
//! subdirectories, each sorted by file name.

use std::path::{Path, PathBuf};

/// Where a discovered extension came from (upstream the
/// global/project scope split).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionOrigin {
    Global,
    Project,
}

/// One discovered extension entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredExtension {
    pub path: PathBuf,
    pub origin: ExtensionOrigin,
}

/// Discover extension files for a project directory (upstream the
/// loader's discovery): global dir first, then project dir; each
/// contributes `*.luau` files and `*/index.luau` subdirectories,
/// sorted by file name.
pub fn discover_extension_files(
    global_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> Vec<DiscoveredExtension> {
    let mut discovered = Vec::new();
    if let Some(global_dir) = global_dir {
        discovered.extend(scan_directory(global_dir, ExtensionOrigin::Global));
    }
    if let Some(project_dir) = project_dir {
        discovered.extend(scan_directory(project_dir, ExtensionOrigin::Project));
    }
    discovered
}

fn scan_directory(dir: &Path, origin: ExtensionOrigin) -> Vec<DiscoveredExtension> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut subdirectories: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirectories.push(path);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("luau") {
            files.push(path);
        }
    }
    files.sort();
    let mut discovered: Vec<DiscoveredExtension> = files
        .into_iter()
        .map(|path| DiscoveredExtension {
            path,
            origin: origin.clone(),
        })
        .collect();
    subdirectories.sort();
    for subdirectory in subdirectories {
        let index = subdirectory.join("index.luau");
        if index.is_file() {
            discovered.push(DiscoveredExtension {
                path: index,
                origin: origin.clone(),
            });
        }
    }
    discovered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree(dir: &Path, files: &[&str], dirs: &[(&str, &str)]) {
        for file in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "return function() end").unwrap();
        }
        for (subdir, file) in dirs {
            let path = dir.join(subdir).join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "return function() end").unwrap();
        }
    }

    /// Upstream discovery: *.luau files and */index.luau, name-sorted
    /// within each scope; global before project.
    #[test]
    fn discovers_files_and_subdirectory_indexes_sorted() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        let project = temp.path().join("project");
        make_tree(
            &global,
            &["b.luau", "a.luau", "ignored.ts"],
            &[("pkg", "index.luau")],
        );
        make_tree(&project, &["z.luau", "a.luau"], &[]);
        let discovered = discover_extension_files(Some(&global), Some(&project));
        let paths: Vec<String> = discovered
            .iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(
            paths,
            vec![
                "a.luau",
                "b.luau",
                "index.luau", // global scope sorted
                "a.luau",
                "z.luau", // project scope sorted
            ]
        );
        assert_eq!(discovered[0].origin, ExtensionOrigin::Global);
        assert_eq!(discovered[2].origin, ExtensionOrigin::Global);
        assert_eq!(discovered[3].origin, ExtensionOrigin::Project);
    }

    /// Missing directories contribute nothing.
    #[test]
    fn missing_directories_contribute_nothing() {
        let discovered = discover_extension_files(None, Some(Path::new("/nonexistent/x")));
        assert!(discovered.is_empty());
    }

    /// A subdirectory without index.luau is skipped.
    #[test]
    fn subdirectory_without_index_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        std::fs::create_dir_all(global.join("pkg")).unwrap();
        std::fs::write(global.join("pkg").join("other.luau"), "").unwrap();
        let discovered = discover_extension_files(Some(&global), None);
        assert!(discovered.is_empty());
    }
}
