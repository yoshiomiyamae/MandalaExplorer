//! Session-only navigation state, independent for each tab.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Default, Clone)]
pub struct Tab {
    pub path: PathBuf,
    pub filter: String,
    offsets: HashMap<PathBuf, f32>,
}

impl Tab {
    pub fn remember(&mut self, path: PathBuf, filter: String, offset: f32) {
        self.offsets.insert(path.clone(), offset.max(0.0));
        self.path = path;
        self.filter = filter;
    }

    pub fn offset(&self, path: &Path) -> f32 {
        self.offsets.get(path).copied().unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returning_to_a_parent_restores_its_scroll() {
        let mut tab = Tab::default();
        tab.remember("parent".into(), "".into(), 720.0);
        tab.remember("parent/child".into(), "".into(), 15.0);
        assert_eq!(tab.offset(std::path::Path::new("parent")), 720.0);
        assert_eq!(tab.offset(std::path::Path::new("unvisited")), 0.0);
    }

    #[test]
    fn tabs_do_not_share_positions_or_filters() {
        let mut first = Tab::default();
        first.remember("folder".into(), "cat".into(), 800.0);
        let mut second = Tab::default();
        second.remember("folder".into(), "dog".into(), 12.0);
        assert_eq!(first.offset(&second.path), 800.0);
        assert_eq!(second.offset(&first.path), 12.0);
        assert_eq!(first.filter, "cat");
    }
}
