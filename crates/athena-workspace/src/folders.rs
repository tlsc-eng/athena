use std::ops::Range;

use crate::{FolderColor, FolderIcon, FolderStyle, Workspace, default_look};

/// A line of the project sidebar: a folder heading, or a project at `index` in `projects`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarRow {
    Folder {
        path: Vec<String>,
        depth: usize,
        collapsed: bool,
        /// Projects inside it, its subfolders' included.
        projects: usize,
    },
    Project {
        index: usize,
        depth: usize,
    },
}

impl SidebarRow {
    pub fn depth(&self) -> usize {
        match self {
            Self::Folder { depth, .. } | Self::Project { depth, .. } => *depth,
        }
    }
}

/// Indices of `folders` (one per project) grouped so each folder's projects and subfolders sit
/// together, in the order each project or subfolder first appears.
fn folder_order(folders: &[&[String]]) -> Vec<usize> {
    fn walk(items: &[(usize, &[String])], depth: usize, out: &mut Vec<usize>) {
        let mut slots: Vec<Result<usize, &str>> = Vec::new();
        for &(i, path) in items {
            match path.get(depth) {
                None => slots.push(Ok(i)),
                Some(name) if !slots.contains(&Err(name.as_str())) => slots.push(Err(name)),
                Some(_) => {}
            }
        }
        for slot in slots {
            match slot {
                Ok(i) => out.push(i),
                Err(name) => {
                    let inside: Vec<(usize, &[String])> = items
                        .iter()
                        .filter(|(_, p)| p.get(depth).map(String::as_str) == Some(name))
                        .copied()
                        .collect();
                    walk(&inside, depth + 1, out);
                }
            }
        }
    }
    let items: Vec<(usize, &[String])> = folders.iter().copied().enumerate().collect();
    let mut out = Vec::with_capacity(folders.len());
    walk(&items, 0, &mut out);
    out
}

impl Workspace {
    /// Reorders `projects` so every folder is one block, keeping the same project active, and
    /// forgets collapsed folders that no longer hold anything.
    pub fn group_folders(&mut self) {
        let order = {
            let folders: Vec<&[String]> = self.projects.iter().map(|p| &p.folder[..]).collect();
            folder_order(&folders)
        };
        if !order.iter().copied().eq(0..self.projects.len()) {
            let active = self.active_project().map(|p| p.root.clone());
            let mut projects: Vec<_> = std::mem::take(&mut self.projects)
                .into_iter()
                .map(Some)
                .collect();
            self.projects = order.iter().filter_map(|&i| projects[i].take()).collect();
            self.active = active.and_then(|root| self.projects.iter().position(|p| p.root == root));
        }
        let projects = &self.projects;
        self.collapsed
            .retain(|path| projects.iter().any(|p| p.folder.starts_with(path)));
        self.folder_styles
            .retain(|s| projects.iter().any(|p| p.folder.starts_with(&s.path)));
    }

    /// The icon and colour `folder` shows: what the user chose, else the defaults for its name.
    pub fn folder_look(&self, folder: &[String]) -> (FolderIcon, FolderColor) {
        let (icon, color) = default_look(folder.last().map_or("", String::as_str));
        match self.folder_style(folder) {
            Some(style) => (style.icon.unwrap_or(icon), style.color.unwrap_or(color)),
            None => (icon, color),
        }
    }

    /// What the user chose for `folder`, if anything.
    pub fn folder_style(&self, folder: &[String]) -> Option<&FolderStyle> {
        self.folder_styles.iter().find(|s| s.path == folder)
    }

    /// Sets `folder`'s icon, or goes back to the default with `None`.
    pub fn set_folder_icon(&mut self, folder: &[String], icon: Option<FolderIcon>) {
        self.edit_style(folder, |s| s.icon = icon);
    }

    /// Sets `folder`'s colour, or goes back to the default with `None`.
    pub fn set_folder_color(&mut self, folder: &[String], color: Option<FolderColor>) {
        self.edit_style(folder, |s| s.color = color);
    }

    fn edit_style(&mut self, folder: &[String], edit: impl FnOnce(&mut FolderStyle)) {
        let at = match self.folder_styles.iter().position(|s| s.path == folder) {
            Some(at) => at,
            None => {
                self.folder_styles.push(FolderStyle {
                    path: folder.to_vec(),
                    icon: None,
                    color: None,
                });
                self.folder_styles.len() - 1
            }
        };
        edit(&mut self.folder_styles[at]);
        let style = &self.folder_styles[at];
        if style.icon.is_none() && style.color.is_none() {
            self.folder_styles.remove(at);
        }
    }

    /// The sidebar's rows, leaving out what collapsed folders hold.
    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        let mut rows = Vec::new();
        let mut open: &[String] = &[];
        // The depth of the collapsed folder whose contents are being skipped.
        let mut hidden_from: Option<usize> = None;
        for (index, project) in self.projects.iter().enumerate() {
            let path = &project.folder[..];
            let shared = open.iter().zip(path).take_while(|(a, b)| a == b).count();
            if hidden_from.is_some_and(|d| shared <= d) {
                hidden_from = None;
            }
            for depth in shared..path.len() {
                if hidden_from.is_some() {
                    break;
                }
                let folder = path[..=depth].to_vec();
                let collapsed = self.collapsed.contains(&folder);
                let projects = self.projects[index..]
                    .iter()
                    .take_while(|p| p.folder.starts_with(&folder))
                    .count();
                rows.push(SidebarRow::Folder {
                    path: folder,
                    depth,
                    collapsed,
                    projects,
                });
                if collapsed {
                    hidden_from = Some(depth);
                }
            }
            open = path;
            if hidden_from.is_none() {
                rows.push(SidebarRow::Project {
                    index,
                    depth: path.len(),
                });
            }
        }
        rows
    }

    /// Every folder that holds a project, parents before children, in sidebar order.
    pub fn folders(&self) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = Vec::new();
        for project in &self.projects {
            for depth in 1..=project.folder.len() {
                let path = project.folder[..depth].to_vec();
                if !out.contains(&path) {
                    out.push(path);
                }
            }
        }
        out
    }

    /// The projects inside `folder`, its subfolders' included; empty when it holds none.
    pub fn folder_range(&self, folder: &[String]) -> Range<usize> {
        let start = self
            .projects
            .iter()
            .position(|p| p.folder.starts_with(folder));
        match start {
            Some(start) => {
                let len = self.projects[start..]
                    .iter()
                    .take_while(|p| p.folder.starts_with(folder))
                    .count();
                start..start + len
            }
            None => 0..0,
        }
    }

    /// Puts the projects in `range` into `folder` (empty for the top level), last in it.
    pub fn move_to_folder(&mut self, range: Range<usize>, folder: &[String]) {
        if range.end > self.projects.len() {
            return;
        }
        for project in &mut self.projects[range.clone()] {
            project.folder = folder.to_vec();
        }
        let end = if folder.is_empty() {
            self.projects.len()
        } else {
            self.projects
                .iter()
                .enumerate()
                .rfind(|(i, p)| p.folder.starts_with(folder) && !range.contains(i))
                .map(|(i, _)| i + 1)
                .unwrap_or(range.end)
        };
        self.move_projects(range, end);
        self.group_folders();
    }

    /// Moves the projects in `range` to just before the project at `before`, into its folder.
    pub fn move_beside(&mut self, range: Range<usize>, before: usize) {
        let Some(folder) = self.projects.get(before).map(|p| p.folder.clone()) else {
            return self.move_to_folder(range, &[]);
        };
        if range.end > self.projects.len() {
            return;
        }
        for project in &mut self.projects[range.clone()] {
            project.folder = folder.clone();
        }
        self.move_projects(range, before);
        self.group_folders();
    }

    /// Renames the last part of `folder`; false when a sibling already has that name.
    pub fn rename_folder(&mut self, folder: &[String], name: &str) -> bool {
        let name = name.trim();
        let Some((_, parent)) = folder.split_last() else {
            return false;
        };
        let mut renamed = parent.to_vec();
        renamed.push(name.to_string());
        if renamed == folder {
            return true;
        }
        if name.is_empty() || self.folders().contains(&renamed) {
            return false;
        }
        self.reparent(folder, &renamed);
        true
    }

    /// Moves `folder` into `parent` (empty for the top level), after what is already there; false
    /// when that would put it inside itself or beside a folder of the same name.
    pub fn move_folder(&mut self, folder: &[String], parent: &[String]) -> bool {
        let Some(name) = folder.last() else {
            return false;
        };
        let mut moved = parent.to_vec();
        moved.push(name.clone());
        if parent.starts_with(folder) || (moved != folder && self.folders().contains(&moved)) {
            return false;
        }
        let range = self.folder_range(folder);
        if range.is_empty() {
            return false;
        }
        let end = match self.folder_range(parent) {
            _ if parent.is_empty() => self.projects.len(),
            inside if inside.is_empty() => range.end,
            inside => inside.end,
        };
        if !(range.start..=range.end).contains(&end) {
            self.move_projects(range, end);
        }
        self.reparent(folder, &moved);
        true
    }

    /// Takes `folder` away, its projects and subfolders moving up into its parent.
    pub fn delete_folder(&mut self, folder: &[String]) {
        let Some((_, parent)) = folder.split_last() else {
            return;
        };
        let depth = parent.len();
        for project in &mut self.projects {
            if project.folder.starts_with(folder) {
                project.folder.remove(depth);
            }
        }
        self.collapsed.retain(|path| path != folder);
        for path in &mut self.collapsed {
            if path.starts_with(folder) {
                path.remove(depth);
            }
        }
        self.folder_styles.retain(|s| s.path != folder);
        // A lifted subfolder that lands on an existing folder gives way to that folder's style.
        let existing: Vec<Vec<String>> = self
            .folder_styles
            .iter()
            .filter(|s| !s.path.starts_with(folder))
            .map(|s| s.path.clone())
            .collect();
        self.folder_styles.retain_mut(|style| {
            if !style.path.starts_with(folder) {
                return true;
            }
            style.path.remove(depth);
            !existing.contains(&style.path)
        });
        self.group_folders();
    }

    pub fn toggle_folder(&mut self, folder: &[String]) {
        match self.collapsed.iter().position(|p| p == folder) {
            Some(i) => {
                self.collapsed.remove(i);
            }
            None => self.collapsed.push(folder.to_vec()),
        }
    }

    /// Opens the folders around the project at `index`, so it can be seen.
    pub fn reveal(&mut self, index: usize) {
        if let Some(project) = self.projects.get(index) {
            let folder = &project.folder;
            self.collapsed.retain(|p| !folder.starts_with(p));
        }
    }

    fn reparent(&mut self, from: &[String], to: &[String]) {
        let swap = |path: &mut Vec<String>| {
            if path.starts_with(from) {
                path.splice(..from.len(), to.iter().cloned());
            }
        };
        for project in &mut self.projects {
            swap(&mut project.folder);
        }
        for path in &mut self.collapsed {
            swap(path);
        }
        for style in &mut self.folder_styles {
            swap(&mut style.path);
        }
        self.group_folders();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Project;
    use std::path::PathBuf;

    fn path(p: &str) -> Vec<String> {
        p.split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }

    fn workspace(projects: &[(&str, &str)]) -> Workspace {
        let mut w: Workspace =
            serde_json::from_str(r#"{"projects":[],"active":null,"window":null}"#).unwrap();
        for (root, folder) in projects {
            let mut p = Project::new(PathBuf::from(root));
            p.folder = path(folder);
            w.projects.push(p);
        }
        w.active = Some(0);
        w
    }

    fn roots(w: &Workspace) -> Vec<&str> {
        w.projects
            .iter()
            .map(|p| p.root.to_str().unwrap())
            .collect()
    }

    fn row(r: &SidebarRow, w: &Workspace) -> String {
        match r {
            SidebarRow::Folder {
                path,
                depth,
                collapsed,
                projects,
            } => format!(
                "{}{}{} ({projects})",
                "  ".repeat(*depth),
                if *collapsed { "▸ " } else { "▾ " },
                path.join("/")
            ),
            SidebarRow::Project { index, depth } => format!(
                "{}{}",
                "  ".repeat(*depth),
                w.projects[*index].root.display()
            ),
        }
    }

    fn rows(w: &Workspace) -> Vec<String> {
        w.sidebar_rows().iter().map(|r| row(r, w)).collect()
    }

    #[test]
    fn folders_gather_in_the_order_they_first_appear() {
        let mut w = workspace(&[
            ("a", "work"),
            ("b", ""),
            ("c", "hobby"),
            ("d", "work/clients"),
            ("e", "work"),
            ("f", "hobby"),
        ]);
        w.active = Some(4);
        w.group_folders();
        assert_eq!(roots(&w), ["a", "d", "e", "b", "c", "f"]);
        assert_eq!(w.active_project().unwrap().root, PathBuf::from("e"));
        assert_eq!(
            rows(&w),
            [
                "▾ work (3)",
                "  a",
                "  ▾ work/clients (1)",
                "    d",
                "  e",
                "b",
                "▾ hobby (2)",
                "  c",
                "  f",
            ]
        );
        assert_eq!(
            w.folders(),
            [path("work"), path("work/clients"), path("hobby")]
        );
    }

    #[test]
    fn a_collapsed_folder_hides_everything_inside_it() {
        let mut w = workspace(&[
            ("a", "work"),
            ("d", "work/clients"),
            ("e", "work"),
            ("b", ""),
        ]);
        w.toggle_folder(&path("work"));
        assert_eq!(rows(&w), ["▸ work (3)", "b"]);
        w.toggle_folder(&path("work"));
        w.toggle_folder(&path("work/clients"));
        assert_eq!(
            rows(&w),
            ["▾ work (3)", "  a", "  ▸ work/clients (1)", "  e", "b"]
        );
        w.reveal(1);
        assert!(w.collapsed.is_empty());
    }

    #[test]
    fn projects_move_into_folders_and_beside_other_projects() {
        let mut w = workspace(&[("a", "work"), ("b", ""), ("c", "")]);
        w.move_to_folder(2..3, &path("work"));
        assert_eq!(roots(&w), ["a", "c", "b"]);
        assert_eq!(w.projects[1].folder, path("work"));
        w.move_beside(2..3, 0);
        assert_eq!(roots(&w), ["b", "a", "c"]);
        assert_eq!(w.projects[0].folder, path("work"));
        w.move_to_folder(0..1, &[]);
        assert_eq!(roots(&w), ["a", "c", "b"]);
        assert!(w.projects[2].folder.is_empty());
        w.move_to_folder(2..3, &path("new/sub"));
        assert_eq!(rows(&w)[3..], ["▾ new (1)", "  ▾ new/sub (1)", "    b"]);
    }

    #[test]
    fn folders_rename_move_and_delete_with_what_they_hold() {
        let mut w = workspace(&[("a", "work"), ("d", "work/clients"), ("b", "hobby")]);
        w.toggle_folder(&path("work/clients"));
        assert!(!w.rename_folder(&path("work"), "hobby"));
        assert!(w.rename_folder(&path("work"), " job "));
        assert_eq!(w.projects[1].folder, path("job/clients"));
        assert_eq!(w.collapsed, [path("job/clients")]);

        assert!(!w.move_folder(&path("job"), &path("job/clients")));
        assert!(w.move_folder(&path("job/clients"), &path("hobby")));
        assert_eq!(roots(&w), ["a", "b", "d"]);
        assert_eq!(w.projects[2].folder, path("hobby/clients"));
        assert_eq!(w.collapsed, [path("hobby/clients")]);
        assert!(w.move_folder(&path("hobby"), &[]));
        assert_eq!(roots(&w), ["a", "b", "d"]);

        w.delete_folder(&path("hobby"));
        assert_eq!(w.projects[1].folder, Vec::<String>::new());
        assert_eq!(w.projects[2].folder, path("clients"));
        assert_eq!(w.collapsed, [path("clients")]);
        assert!(
            w.rename_folder(&path("clients"), "clients"),
            "an unchanged name is fine"
        );

        w.delete_folder(&path("clients"));
        assert!(
            w.collapsed.is_empty(),
            "a removed folder is not left collapsed"
        );
        let mut nested = workspace(&[("a", "work"), ("b", "work/clients")]);
        nested.toggle_folder(&path("work/clients"));
        nested.delete_folder(&path("work/clients"));
        assert!(
            nested.collapsed.is_empty(),
            "its parent does not take its collapse"
        );
    }

    #[test]
    fn collapsed_folders_that_empty_out_are_forgotten() {
        let mut w = workspace(&[("a", "work"), ("b", "")]);
        w.toggle_folder(&path("work"));
        w.move_to_folder(0..1, &[]);
        assert!(w.collapsed.is_empty());
    }

    fn styled(w: &mut Workspace, folder: &str, icon: FolderIcon, color: FolderColor) {
        w.set_folder_icon(&path(folder), Some(icon));
        w.set_folder_color(&path(folder), Some(color));
    }

    fn style_paths(w: &Workspace) -> Vec<Vec<String>> {
        w.folder_styles.iter().map(|s| s.path.clone()).collect()
    }

    #[test]
    fn a_folder_look_is_the_choice_over_the_default_and_a_cleared_part_goes_back() {
        let mut w = workspace(&[("a", "work")]);
        let work = path("work");
        let (icon, color) = default_look("work");
        assert_eq!(w.folder_look(&work), (icon, color));
        w.set_folder_icon(&work, Some(FolderIcon::Rocket));
        assert_eq!(w.folder_look(&work), (FolderIcon::Rocket, color));
        w.set_folder_color(&work, Some(FolderColor::Cyan));
        assert_eq!(
            w.folder_look(&work),
            (FolderIcon::Rocket, FolderColor::Cyan)
        );
        w.set_folder_icon(&work, None);
        assert_eq!(w.folder_look(&work), (icon, FolderColor::Cyan));
        w.set_folder_color(&work, None);
        assert!(
            w.folder_styles.is_empty(),
            "only customised folders are kept"
        );
    }

    #[test]
    fn default_looks_are_stable_and_mostly_differ_between_names() {
        assert_eq!(default_look("work"), default_look("work"));
        // Pinned so a change of hash, which would reshuffle every saved layout, is deliberate.
        assert_eq!(
            default_look("work"),
            (FolderIcon::Gear, FolderColor::Accent)
        );
        let names = [
            "work", "hobby", "clients", "oss", "infra", "docs", "labs", "home",
        ];
        let mut looks: Vec<_> = names.iter().map(|n| default_look(n)).collect();
        looks.sort_by_key(|(i, c)| (*i as u8, *c as u8));
        looks.dedup();
        assert!(looks.len() >= names.len() - 1, "{looks:?}");
    }

    #[test]
    fn folder_styles_follow_renames_and_moves_with_their_subfolders() {
        let mut w = workspace(&[("a", "work"), ("d", "work/clients"), ("b", "hobby")]);
        styled(&mut w, "work", FolderIcon::Flask, FolderColor::Red);
        styled(&mut w, "work/clients", FolderIcon::Book, FolderColor::Green);
        styled(&mut w, "hobby", FolderIcon::Leaf, FolderColor::Blue);

        assert!(w.rename_folder(&path("work"), "job"));
        assert_eq!(
            style_paths(&w),
            [path("job"), path("job/clients"), path("hobby")]
        );
        assert_eq!(
            w.folder_look(&path("job")),
            (FolderIcon::Flask, FolderColor::Red)
        );

        assert!(w.move_folder(&path("job"), &path("hobby")));
        assert_eq!(
            style_paths(&w),
            [path("hobby/job"), path("hobby/job/clients"), path("hobby")]
        );
        assert_eq!(
            w.folder_look(&path("hobby/job/clients")),
            (FolderIcon::Book, FolderColor::Green)
        );
    }

    #[test]
    fn deleting_a_folder_drops_its_style_and_lifts_its_subfolders() {
        let mut w = workspace(&[("a", "work"), ("d", "work/clients"), ("b", "clients")]);
        styled(&mut w, "work", FolderIcon::Flask, FolderColor::Red);
        styled(&mut w, "work/clients", FolderIcon::Book, FolderColor::Green);
        w.delete_folder(&path("work"));
        assert_eq!(style_paths(&w), [path("clients")]);
        assert_eq!(w.folder_styles[0].icon, Some(FolderIcon::Book));

        for lifted_first in [false, true] {
            let mut merged = workspace(&[("d", "work/clients"), ("b", "clients")]);
            let mut order = [
                ("clients", FolderIcon::Star, FolderColor::Cyan),
                ("work/clients", FolderIcon::Book, FolderColor::Green),
            ];
            if lifted_first {
                order.reverse();
            }
            for (folder, icon, color) in order {
                styled(&mut merged, folder, icon, color);
            }
            merged.delete_folder(&path("work"));
            assert_eq!(
                merged.folder_styles,
                [FolderStyle {
                    path: path("clients"),
                    icon: Some(FolderIcon::Star),
                    color: Some(FolderColor::Cyan),
                }],
                "the folder already there keeps its own style, whichever is listed first"
            );
        }
    }

    #[test]
    fn styles_of_folders_that_empty_out_are_forgotten() {
        let mut w = workspace(&[("a", "work"), ("b", "work/clients"), ("c", "")]);
        styled(&mut w, "work", FolderIcon::Flask, FolderColor::Red);
        styled(&mut w, "work/clients", FolderIcon::Book, FolderColor::Green);
        w.move_to_folder(1..2, &[]);
        assert_eq!(style_paths(&w), [path("work")]);
        w.move_to_folder(0..1, &[]);
        assert!(w.folder_styles.is_empty());
    }

    #[test]
    fn detaching_a_folders_last_project_forgets_its_style() {
        let mut w = workspace(&[("a", "work"), ("b", "")]);
        styled(&mut w, "work", FolderIcon::Flask, FolderColor::Red);
        assert!(w.detach_project(0).is_some());
        assert!(w.folder_styles.is_empty());
    }
}
