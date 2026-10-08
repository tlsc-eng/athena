use gpui::{Action, App, KeyBinding, Menu, MenuItem, SystemMenuType, actions};

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
    ]
);

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = athena, no_json)]
pub struct SelectProject(pub usize);

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = athena, no_json)]
pub struct SelectTab(pub usize);

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
        KeyBinding::new("cmd-b", ToggleFileTree, None),
        KeyBinding::new("cmd-j", ToggleNotifications, None),
        KeyBinding::new("cmd-shift-t", NewClaudeSession, None),
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

    cx.set_menus(vec![
        Menu {
            name: "Athena".into(),
            items: vec![
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
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseTab),
                MenuItem::action("Close Project", CloseProject),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Command Palette", CommandPalette),
                MenuItem::action("Go to File…", QuickOpen),
                MenuItem::action("Toggle File Tree", ToggleFileTree),
                MenuItem::action("Notifications", ToggleNotifications),
                MenuItem::action("Containers", ShowContainers),
                MenuItem::action("Playwright", ShowPlaywright),
                MenuItem::separator(),
                MenuItem::action("Split Right", SplitRight),
                MenuItem::action("Split Down", SplitDown),
                MenuItem::action("Zoom Pane", TogglePaneZoom),
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
    ]);
}
