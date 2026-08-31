use std::path::{Component, Path, PathBuf};

pub(crate) mod claude;
pub(crate) mod pi;

pub(crate) fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}
