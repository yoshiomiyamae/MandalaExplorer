//! Selection is path-based so sorting cannot move it to another file.
use std::{collections::HashSet, path::PathBuf};

#[derive(Default)]
pub struct Selection {
    pub paths: HashSet<PathBuf>,
    anchor: Option<PathBuf>,
}

impl Selection {
    pub fn click(&mut self, paths: &[PathBuf], index: usize, ctrl: bool, shift: bool) {
        let Some(path) = paths.get(index) else { return };
        if shift {
            let anchor = self
                .anchor
                .as_ref()
                .and_then(|a| paths.iter().position(|p| p == a))
                .unwrap_or(index);
            if !ctrl {
                self.paths.clear();
            }
            self.paths.extend(paths[anchor.min(index)..=anchor.max(index)].iter().cloned());
        } else {
            if !ctrl {
                self.paths.clear();
            }
            if !self.paths.insert(path.clone()) {
                self.paths.remove(path);
            }
            self.anchor = Some(path.clone());
        }
    }
}

pub fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| c < ' ' || "<>:\"/\\|?*".contains(c))
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        && !stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.chars().count() == 1 && "123456789¹²³".contains(suffix))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn control_toggles_and_shift_extends_from_the_anchor() {
        let paths: Vec<_> =
            ["a", "b", "c", "d"].into_iter().map(std::path::PathBuf::from).collect();
        let mut selection = Selection::default();
        selection.click(&paths, 1, false, false);
        selection.click(&paths, 3, true, false);
        assert_eq!(selection.paths.len(), 2);
        selection.click(&paths, 3, true, false);
        assert_eq!(selection.paths.len(), 1);
        selection.click(&paths, 0, false, true);
        assert_eq!(selection.paths.len(), 4);
    }
    #[test]
    fn rename_rejects_paths_and_windows_reserved_names() {
        for name in ["", "..", "a/b", "a\\b", "CON.txt", "nul", "x.", "x ", "a:b", "COM¹.txt"] {
            assert!(!valid_name(name), "{name}");
        }
        assert!(valid_name("猫の写真.jpg"));
    }
}
