use std::path::{Path, PathBuf};

use athena_editor as editor;
use gpui::{Action, App, Global, KeyBinding, Menu, MenuItem, SystemMenuType, actions};

actions!(
    athena,
    [
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        ToggleFullScreen,
        AddProject,
        CloseProject,
        PrevProject,
        NextProject,
        NewTerminal,
        CloseTab,
        SplitRight,
        SplitDown,
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        TogglePaneZoom,
        NextTab,
        PrevTab,
        QuickOpen,
        QuickOpenBeside,
        CommandPalette,
        ToggleFileTree,
        ToggleNotifications,
        ShowContainers,
        NewClaudeSession,
        ChangeClaudeCommand,
        EnableClaudeHooks,
        DisableClaudeHooks,
        ShowPlaywright,
        RunPlaywright,
        EnablePlaywrightMcp,
        DisablePlaywrightMcp,
        NewPreview,
        TogglePreview,
        SaveAs,
        ToggleAutoSave,
        ToggleFormatOnSave,
        ToggleWordWrapDefault,
        FindInProject,
        ShowChanges,
        ToggleBlame,
        SwitchBranch,
        NavigateBack,
        NavigateForward,
        RevealInTree,
        FontZoomIn,
        FontZoomOut,
        FontZoomReset,
        ShowProblems,
        NextProblem,
        PrevProblem,
        GoToSymbol,
        GoToWorkspaceSymbol,
        ToggleIdeIntegration,
        SendToClaude,
        OpenRecent,
        ClearRecent,
        ThemeFollowSystem,
        ThemeLight,
        ThemeDark,
        OpenKeyboardShortcuts,
        OpenSettings,
        ToggleInlayHints,
        ToggleTerminalPanel,
        NewPanelTerminal,
        MoveTerminalToPanel,
        MoveTerminalToEditor,
    ]
);

actions!(
    athena,
    [
        GitFetch,
        GitPull,
        GitPush,
        GitStash,
        GitStashIncludeUntracked,
        GitPopStash,
        RunTask,
        ShowTests,
        RunTestAtCursor,
        RunTestsInFile,
        RunAllTests,
        RerunFailedTests,
        StopTests,
    ]
);

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = athena, no_json)]
pub struct SelectProject(pub usize);

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = athena, no_json)]
pub struct SelectTab(pub usize);

/// A recently closed project folder, from the File menu.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = athena, no_json)]
pub struct OpenRecentProject(pub PathBuf);

/// The entry of the open code action menu to run.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = athena, no_json)]
pub struct ApplyCodeAction(pub usize);

pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());

    let mut bindings = vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
        KeyBinding::new("cmd-o", AddProject, None),
        KeyBinding::new("cmd-shift-w", CloseProject, None),
        KeyBinding::new("cmd-alt-[", PrevProject, None),
        KeyBinding::new("cmd-alt-]", NextProject, None),
        KeyBinding::new("cmd-t", NewTerminal, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("cmd-d", SplitRight, None),
        KeyBinding::new("cmd-shift-d", SplitDown, None),
        KeyBinding::new("cmd-alt-left", FocusPaneLeft, None),
        KeyBinding::new("cmd-alt-right", FocusPaneRight, None),
        KeyBinding::new("cmd-alt-up", FocusPaneUp, None),
        KeyBinding::new("cmd-alt-down", FocusPaneDown, None),
        KeyBinding::new("cmd-shift-enter", TogglePaneZoom, None),
        KeyBinding::new("cmd-}", NextTab, None),
        KeyBinding::new("cmd-{", PrevTab, None),
        KeyBinding::new("cmd-p", QuickOpen, None),
        KeyBinding::new("cmd-shift-p", CommandPalette, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-b", ToggleFileTree, None),
        KeyBinding::new("cmd-j", ToggleNotifications, None),
        KeyBinding::new("ctrl-`", ToggleTerminalPanel, None),
        // macOS reports ⌃⇧` as ⌃~, so that is the binding that matches; the other is for the menu.
        KeyBinding::new("ctrl-~", NewPanelTerminal, None),
        KeyBinding::new("ctrl-shift-`", NewPanelTerminal, None),
        KeyBinding::new("cmd-shift-t", NewClaudeSession, None),
        KeyBinding::new("cmd-shift-v", TogglePreview, None),
        KeyBinding::new("cmd-shift-s", SaveAs, None),
        KeyBinding::new("cmd-shift-f", FindInProject, None),
        KeyBinding::new("cmd-alt-shift-g", ToggleBlame, None),
        KeyBinding::new("ctrl--", NavigateBack, None),
        // macOS reports ⌃⇧- as ⌃_, so that is the binding that matches; the other is for the menu.
        KeyBinding::new("ctrl-_", NavigateForward, None),
        KeyBinding::new("ctrl-shift--", NavigateForward, None),
        // The image viewer's own zoom keys win while it has focus, being bound deeper.
        KeyBinding::new("cmd-=", FontZoomIn, None),
        KeyBinding::new("cmd-+", FontZoomIn, None),
        KeyBinding::new("cmd--", FontZoomOut, None),
        KeyBinding::new("cmd-0", FontZoomReset, None),
        KeyBinding::new("cmd-shift-m", ShowProblems, None),
        KeyBinding::new("cmd-shift-o", GoToSymbol, None),
        // VS Code's ⌘T opens a terminal here, so workspace symbols take ⌘⌥O.
        KeyBinding::new("cmd-alt-o", GoToWorkspaceSymbol, None),
        // The Claude Code extension's key for "Insert At-Mention" in VS Code.
        KeyBinding::new("cmd-alt-k", SendToClaude, Some("Editor")),
        // Only in the editor: in a terminal F8 belongs to the program running there.
        KeyBinding::new("f8", NextProblem, Some("Editor")),
        KeyBinding::new("shift-f8", PrevProblem, Some("Editor")),
        // VS Code's key, except in a terminal, where the shell's reverse search needs it.
        KeyBinding::new("ctrl-r", OpenRecent, Some("!Terminal")),
    ];
    for n in 1..=9 {
        bindings.push(KeyBinding::new(
            &format!("cmd-alt-{n}"),
            SelectProject(n - 1),
            None,
        ));
        bindings.push(KeyBinding::new(&format!("cmd-{n}"), SelectTab(n - 1), None));
    }
    cx.bind_keys(bindings);
    sync_recent_menu(&[], cx);
}

/// The recent folders the File menu was last built with.
struct RecentMenu(Vec<PathBuf>);

impl Global for RecentMenu {}

/// Rebuilds the menu bar when the recently closed folders changed.
pub fn sync_recent_menu(recent: &[PathBuf], cx: &mut App) {
    if cx
        .try_global::<RecentMenu>()
        .is_some_and(|shown| shown.0 == recent)
    {
        return;
    }
    rebuild_menus(recent, cx);
}

/// Sets the menu bar again, which also refreshes the shortcuts it shows.
pub fn rebuild_menus(recent: &[PathBuf], cx: &mut App) {
    cx.set_global(RecentMenu(recent.to_vec()));
    cx.set_menus(menus(recent));
}

/// A folder as Open Recent shows it, with the home folder as `~`.
pub fn display_path(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) && path != home => {
            format!("~/{}", path.strip_prefix(&home).unwrap_or(path).display())
        }
        _ => path.display().to_string(),
    }
}

fn recent_menu(recent: &[PathBuf]) -> Menu {
    let mut items: Vec<MenuItem> = recent
        .iter()
        .map(|root| MenuItem::action(display_path(root), OpenRecentProject(root.clone())))
        .collect();
    if !items.is_empty() {
        items.push(MenuItem::separator());
    }
    items.push(MenuItem::action("More…", OpenRecent));
    items.push(MenuItem::action("Clear Recently Opened", ClearRecent));
    Menu {
        name: "Open Recent".into(),
        items,
    }
}

fn menus(recent: &[PathBuf]) -> Vec<Menu> {
    vec![
        Menu {
            name: "Athena".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide Athena", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit Athena", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Terminal", NewTerminal),
                MenuItem::action("New Claude Session", NewClaudeSession),
                MenuItem::action("Open Project…", AddProject),
                MenuItem::submenu(recent_menu(recent)),
                MenuItem::separator(),
                MenuItem::action("Save As…", SaveAs),
                MenuItem::action("Toggle Auto Save", ToggleAutoSave),
                MenuItem::action("Toggle Format on Save", ToggleFormatOnSave),
                MenuItem::action("Toggle Word Wrap by Default", ToggleWordWrapDefault),
                MenuItem::action("Toggle Claude Code Integration", ToggleIdeIntegration),
                MenuItem::action("Keyboard Shortcuts", OpenKeyboardShortcuts),
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseTab),
                MenuItem::action("Close Project", CloseProject),
            ],
        },
        Menu {
            name: "Selection".into(),
            items: vec![
                MenuItem::action("Select All", editor::SelectAll),
                MenuItem::separator(),
                MenuItem::action("Copy Line Up", editor::CopyLinesUp),
                MenuItem::action("Copy Line Down", editor::CopyLinesDown),
                MenuItem::action("Move Line Up", editor::MoveLinesUp),
                MenuItem::action("Move Line Down", editor::MoveLinesDown),
                MenuItem::separator(),
                MenuItem::action("Add Cursor Above", editor::AddCursorAbove),
                MenuItem::action("Add Cursor Below", editor::AddCursorBelow),
                MenuItem::action("Add Next Occurrence", editor::AddNextOccurrence),
                MenuItem::action("Skip to Next Occurrence", editor::SkipOccurrence),
                MenuItem::action("Select All Occurrences", editor::SelectAllOccurrences),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Command Palette", CommandPalette),
                MenuItem::action("Go to File…", QuickOpen),
                MenuItem::action("Go to Symbol in File…", GoToSymbol),
                MenuItem::action("Go to Symbol in Workspace…", GoToWorkspaceSymbol),
                MenuItem::action("Back", NavigateBack),
                MenuItem::action("Forward", NavigateForward),
                MenuItem::action("Find in Project…", FindInProject),
                MenuItem::action("Toggle File Tree", ToggleFileTree),
                MenuItem::action("Reveal Active File in Tree", RevealInTree),
                MenuItem::action("Problems", ShowProblems),
                MenuItem::action("Terminal", ToggleTerminalPanel),
                MenuItem::action("New Terminal in Panel", NewPanelTerminal),
                MenuItem::action("Move Terminal into Panel", MoveTerminalToPanel),
                MenuItem::action("Move Terminal into Editor Area", MoveTerminalToEditor),
                MenuItem::action("Notifications", ToggleNotifications),
                MenuItem::action("Containers", ShowContainers),
                MenuItem::action("Playwright", ShowPlaywright),
                MenuItem::action("Source Control Changes", ShowChanges),
                MenuItem::action("Toggle Inline Blame", ToggleBlame),
                MenuItem::action("New Browser Preview", NewPreview),
                MenuItem::action("Open Markdown Preview", TogglePreview),
                MenuItem::action("Word Wrap", editor::ToggleWordWrap),
                MenuItem::separator(),
                MenuItem::action("Split Right", SplitRight),
                MenuItem::action("Split Down", SplitDown),
                MenuItem::action("Zoom Pane", TogglePaneZoom),
                MenuItem::separator(),
                MenuItem::action("Zoom In", FontZoomIn),
                MenuItem::action("Zoom Out", FontZoomOut),
                MenuItem::action("Reset Zoom", FontZoomReset),
                MenuItem::submenu(Menu {
                    name: "Theme".into(),
                    items: vec![
                        MenuItem::action("Follow System Appearance", ThemeFollowSystem),
                        MenuItem::action("Light", ThemeLight),
                        MenuItem::action("Dark", ThemeDark),
                    ],
                }),
                MenuItem::separator(),
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PrevTab),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
                MenuItem::action("Toggle Full Screen", ToggleFullScreen),
                MenuItem::separator(),
                MenuItem::action("Previous Project", PrevProject),
                MenuItem::action("Next Project", NextProject),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use gpui::{KeyBindingContextPredicate, KeyContext, Keystroke};

    use super::{display_path, menus};

    #[test]
    fn navigation_keystrokes_parse() {
        for source in ["ctrl--", "ctrl-_", "ctrl-shift--"] {
            let k = Keystroke::parse(source).unwrap();
            assert!(k.modifiers.control, "{source}");
        }
        assert_eq!(Keystroke::parse("ctrl--").unwrap().key, "-");
        assert_eq!(Keystroke::parse("ctrl-_").unwrap().key, "_");
    }

    #[test]
    fn navigation_and_lsp_keystrokes_parse() {
        for (source, key) in [
            ("f8", "f8"),
            ("shift-f8", "f8"),
            ("f2", "f2"),
            ("cmd-f12", "f12"),
            ("cmd-.", "."),
            ("cmd-shift-m", "m"),
            ("cmd-shift-o", "o"),
            ("cmd-alt-o", "o"),
        ] {
            assert_eq!(Keystroke::parse(source).unwrap().key, key, "{source}");
        }
    }

    #[test]
    fn open_recent_leaves_ctrl_r_to_the_terminal() {
        let only_outside = KeyBindingContextPredicate::parse("!Terminal").unwrap();
        let stack = |names: &[&str]| -> Vec<KeyContext> {
            names
                .iter()
                .map(|n| KeyContext::parse(n).unwrap())
                .collect()
        };
        assert!(
            only_outside
                .depth_of(&stack(&["Shell", "Editor"]))
                .is_some()
        );
        assert!(
            only_outside
                .depth_of(&stack(&["Shell", "Terminal"]))
                .is_none()
        );
    }

    #[test]
    fn recent_folders_under_home_are_shown_with_a_tilde() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(display_path(&home.join("code/x")), "~/code/x");
        assert_eq!(display_path(Path::new("/opt/x")), "/opt/x");
    }

    #[test]
    fn the_selection_menu_sits_between_file_and_view_with_the_multi_cursor_commands() {
        let menus = menus(&[]);
        let names: Vec<&str> = menus.iter().map(|m| m.name.as_ref()).collect();
        assert_eq!(names, ["Athena", "File", "Selection", "View", "Window"]);
        let selection: Vec<&str> = menus[2]
            .items
            .iter()
            .filter_map(|item| match item {
                gpui::MenuItem::Action { name, .. } => Some(name.as_ref()),
                _ => None,
            })
            .collect();
        for name in [
            "Add Cursor Above",
            "Add Cursor Below",
            "Add Next Occurrence",
            "Skip to Next Occurrence",
            "Select All Occurrences",
        ] {
            assert!(selection.contains(&name), "{name}");
        }
    }

    #[test]
    fn terminal_panel_keystrokes_parse() {
        for (source, key, shift) in [
            ("ctrl-`", "`", false),
            ("ctrl-~", "~", false),
            ("ctrl-shift-`", "`", true),
        ] {
            let k = Keystroke::parse(source).unwrap();
            assert!(k.modifiers.control && !k.modifiers.platform, "{source}");
            assert_eq!(
                (k.key.as_str(), k.modifiers.shift),
                (key, shift),
                "{source}"
            );
        }
    }

    #[test]
    fn font_zoom_keystrokes_parse() {
        for (source, key) in [
            ("cmd-=", "="),
            ("cmd-+", "+"),
            ("cmd--", "-"),
            ("cmd-0", "0"),
        ] {
            let k = Keystroke::parse(source).unwrap();
            assert!(k.modifiers.platform, "{source}");
            assert_eq!(k.key, key, "{source}");
        }
    }
}
