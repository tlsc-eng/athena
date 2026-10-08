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
    ]
);

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = athena, no_json)]
pub struct SelectProject(pub usize);

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
    ];
    for n in 1..=9 {
        bindings.push(KeyBinding::new(
            &format!("cmd-alt-{n}"),
            SelectProject(n - 1),
            None,
        ));
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
                MenuItem::action("Open Project…", AddProject),
                MenuItem::action("Close Project", CloseProject),
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
