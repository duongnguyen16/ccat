use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntryItem {
    pub name: String,
    pub path: PathBuf,
    pub is_parent: bool,
}

/// Clean a path to remove Windows `\\?\` verbatim UNC prefixes and canonicalize
pub fn clean_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| dunce::simplified(path).to_path_buf())
}

/// Convert path to display string, guaranteed free of `\\?\`
pub fn display_path(path: &Path) -> String {
    let cleaned = dunce::simplified(path);
    let s = cleaned.to_string_lossy().to_string();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        stripped.to_string()
    } else {
        s
    }
}

/// Open native OS folder picker dialog (Windows FileDialog, macOS NSOpenPanel, Linux portal)
pub fn open_native_folder_picker(default_path: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Select Working Directory");
    if let Some(path) = default_path {
        let cleaned = clean_path(path);
        if cleaned.exists() {
            dialog = dialog.set_directory(cleaned);
        }
    }
    dialog.pick_folder().map(|p| clean_path(&p))
}

/// Resolve initial path from CLI argument or current directory
pub fn resolve_initial_dir(dir_arg: Option<&str>) -> (PathBuf, bool) {
    match dir_arg {
        Some(d) if d == "." => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            (clean_path(&cwd), true)
        }
        Some(d) => {
            let path = PathBuf::from(d);
            if path.exists() && path.is_dir() {
                (clean_path(&path), true)
            } else {
                (path, true)
            }
        }
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            (clean_path(&cwd), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_initial_dir_dot() {
        let (path, explicit) = resolve_initial_dir(Some("."));
        assert!(explicit);
        assert!(path.exists());
        let str_rep = display_path(&path);
        assert!(!str_rep.starts_with(r"\\?\"));
    }

    #[test]
    fn test_display_path_strips_unc() {
        let p = PathBuf::from(r"\\?\D:\Dev\ccat");
        let display = display_path(&p);
        assert_eq!(display, r"D:\Dev\ccat");
    }

    #[test]
    fn test_resolve_initial_dir_none() {
        let (path, explicit) = resolve_initial_dir(None);
        assert!(!explicit);
        assert!(path.exists());
    }
}
