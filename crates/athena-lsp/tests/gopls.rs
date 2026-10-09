use std::time::{Duration, Instant};

use athena_lsp::{
    Client, CodeLens, Event, FileChange, MarkupBlock, Position, Range, RenameTarget, SemanticReply,
    ServerKind, Severity, apply_semantic_edits, apply_text_edits, decode_semantic_tokens,
};

fn next_event(events: &async_channel::Receiver<Event>, deadline: Instant) -> Option<Event> {
    while Instant::now() < deadline {
        if let Ok(event) = events.try_recv() {
            return Some(event);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn gopls_reports_an_unused_import_and_answers_lookups_hover_completion_formatting_and_signatures() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("athena-lsp-go-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("go.mod"), "module example.com/probe\n\ngo 1.21\n").unwrap();
    let source = "package main\n\nimport \"os\"\n\nfunc helper() int { return 1 }\n\nfunc main() { _ = helper() }\n";
    let file = dir.join("main.go");
    std::fs::write(&file, source).unwrap();

    let (client, events) = Client::start(ServerKind::Go, dir.clone());
    client.did_open(&file, "go", 1, source.into());

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut unused = None;
    while unused.is_none() {
        match next_event(&events, deadline).expect("no diagnostics from gopls in time") {
            Event::Diagnostics { path, list } if path == file => {
                unused = list.into_iter().find(|d| d.message.contains("\"os\""));
            }
            Event::Stopped(why) => panic!("gopls stopped: {why}"),
            _ => {}
        }
    }
    let unused = unused.unwrap();
    assert_eq!(unused.severity, Severity::Error);
    assert_eq!(unused.range.start.line, 2);

    let call = Position {
        line: 6,
        character: 19,
    };
    let found = futures_lite_block_on(client.definition(&file, call)).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, file);
    assert_eq!(found[0].range.start.line, 4);

    let mut uses = futures_lite_block_on(client.references(&file, call)).unwrap();
    uses.sort_by_key(|l| l.range.start);
    let lines: Vec<u32> = uses.iter().map(|l| l.range.start.line).collect();
    assert_eq!(lines, [4, 6], "the declaration and the call");
    assert!(uses.iter().all(|l| l.path == file));

    let hover = futures_lite_block_on(client.hover(&file, call))
        .unwrap()
        .expect("gopls describes helper");
    assert!(
        hover
            .blocks
            .iter()
            .any(|b| matches!(b, MarkupBlock::Code(code) if code.contains("func helper() int"))),
        "{hover:?}"
    );
    assert_eq!(hover.range.map(|r| r.start.line), Some(6));

    assert!(client.completion_triggers().iter().any(|c| c == "."));
    let edited = "package main\n\nimport \"fmt\"\n\nfunc helper() int { return 1 }\n\nfunc main() { fmt.Pri; hel }\n";
    client.did_change(&file, 2, edited.into());
    let after_pri = Position {
        line: 6,
        character: 21,
    };
    let list = futures_lite_block_on(client.completion(&file, after_pri, None)).unwrap();
    let println = list
        .items
        .iter()
        .find(|i| i.label == "Println")
        .expect("fmt.Println is offered");
    let range = println.range.expect("gopls sends a text edit");
    assert_eq!((range.start.character, range.end.character), (18, 21));
    assert!(println.text.starts_with("Println"));
    let word_end = Position {
        line: 6,
        character: 26,
    };
    let list = futures_lite_block_on(client.completion(&file, word_end, None)).unwrap();
    assert!(list.items.iter().any(|i| i.label == "helper"), "{list:?}");

    let messy =
        "package main\n\nfunc  helper( ) int { return 1 }\n\nfunc main() { _ = helper() }\n";
    client.did_change(&file, 3, messy.into());
    let edits = futures_lite_block_on(client.formatting(&file, 4, false)).unwrap();
    assert!(!edits.is_empty(), "gofmt has spaces to take out");
    assert!(edits.iter().all(|e| e.range.start.line == 2), "{edits:?}");

    assert!(client.signature_triggers().iter().any(|c| c == "("));
    let call = "package main\n\nimport \"strings\"\n\nfunc main() { _ = strings.Join(nil, ) }\n";
    client.did_change(&file, 4, call.into());
    let second_argument = Position {
        line: 4,
        character: 36,
    };
    let help = futures_lite_block_on(client.signature_help(&file, second_argument))
        .unwrap()
        .expect("inside a call");
    assert!(help.label.contains("Join("), "{help:?}");
    assert_eq!(&help.label[help.active.clone().unwrap()], "sep string");

    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_killed_gopls_reports_stopped_and_a_new_one_reports_the_open_file_again() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("athena-lsp-crash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("go.mod"), "module example.com/crash\n\ngo 1.21\n").unwrap();
    let source = "package main\n\nimport \"os\"\n\nfunc main() {}\n";
    let file = dir.join("main.go");
    std::fs::write(&file, source).unwrap();

    let reports_unused_import = |client: &Client, events: &async_channel::Receiver<Event>| {
        client.did_open(&file, "go", 1, source.into());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match next_event(events, deadline).expect("no diagnostics from gopls in time") {
                Event::Diagnostics { path, list }
                    if path == file && list.iter().any(|d| d.message.contains("\"os\"")) =>
                {
                    return;
                }
                Event::Stopped(why) => panic!("gopls stopped: {why}"),
                _ => {}
            }
        }
    };

    let (client, events) = Client::start(ServerKind::Go, dir.clone());
    reports_unused_import(&client, &events);
    // gopls may run under a version manager's shim, so the process tree goes, as in a crash.
    let mut tree = vec![client.pid().expect("gopls is running").to_string()];
    let mut i = 0;
    while i < tree.len() {
        let children = std::process::Command::new("pgrep")
            .args(["-P", &tree[i]])
            .output()
            .unwrap();
        tree.extend(
            String::from_utf8_lossy(&children.stdout)
                .split_whitespace()
                .map(String::from),
        );
        i += 1;
    }
    let killed = std::process::Command::new("kill")
        .arg("-KILL")
        .args(&tree)
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match next_event(&events, deadline).expect("no Stopped after gopls was killed") {
            Event::Stopped(_) => break,
            _ => continue,
        }
    }
    let at = Position {
        line: 4,
        character: 5,
    };
    assert!(futures_lite_block_on(client.hover(&file, at)).is_err());
    drop(client);

    let (client, events) = Client::start(ServerKind::Go, dir.clone());
    reports_unused_import(&client, &events);
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gopls_renames_fixes_imports_lists_symbols_and_finds_implementations() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("athena-lsp-edits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shapes")).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("go.mod"), "module example.com/edits\n\ngo 1.21\n").unwrap();
    let shapes = "package shapes\n\ntype Shape interface {\n\tArea() float64\n}\n\ntype Square struct {\n\tSide float64\n}\n\nfunc (s Square) Area() float64 { return s.Side * s.Side }\n";
    let shapes_file = dir.join("shapes/shapes.go");
    std::fs::write(&shapes_file, shapes).unwrap();
    let main = "package main\n\nimport \"example.com/edits/shapes\"\n\nfunc total(list []shapes.Shape) float64 {\n\tsum := 0.0\n\tfor _, s := range list {\n\t\tsum += s.Area()\n\t}\n\treturn sum\n}\n\nfunc main() {\n\tfmt.Println(total([]shapes.Shape{shapes.Square{Side: 2}}))\n}\n";
    let main_file = dir.join("main.go");
    std::fs::write(&main_file, main).unwrap();

    let (client, events) = Client::start(ServerKind::Go, dir.clone());
    client.did_open(&main_file, "go", 1, main.into());
    let deadline = Instant::now() + Duration::from_secs(30);
    let undefined = loop {
        match next_event(&events, deadline).expect("no diagnostics from gopls in time") {
            Event::Diagnostics { path, list } if path == main_file => {
                if let Some(d) = list.into_iter().find(|d| d.message.contains("fmt")) {
                    break d;
                }
            }
            Event::Stopped(why) => panic!("gopls stopped: {why}"),
            _ => {}
        }
    };
    assert!(client.supports("/renameProvider/prepareProvider"));

    // The quick fix for an undefined package is an import, as an edit or a command.
    let fixes = futures_lite_block_on(client.code_actions(
        &main_file,
        undefined.range,
        vec![undefined.raw.clone()],
        Some(&["quickfix"]),
    ))
    .unwrap();
    let import = fixes
        .iter()
        .find(|a| a.title.contains("\"fmt\""))
        .unwrap_or_else(|| panic!("no import fix in {fixes:?}"));
    assert!(import.is_quickfix());
    let import = if import.needs_resolve() {
        futures_lite_block_on(client.resolve_code_action(import)).unwrap()
    } else {
        import.clone()
    };
    let edits = match import.edit.as_ref().map(|e| e.changes.as_slice()) {
        Some([FileChange::Edit { path, edits, .. }]) if *path == main_file => edits.clone(),
        other => panic!("unexpected import fix {other:?} ({import:?})"),
    };
    let fixed = apply_text_edits(main, &edits).unwrap();
    assert!(fixed.contains("\"fmt\""), "{fixed}");

    let organize = futures_lite_block_on(client.code_actions(
        &main_file,
        Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
        Vec::new(),
        Some(&["source.organizeImports"]),
    ))
    .unwrap();
    assert!(
        organize
            .iter()
            .all(|a| a.kind.as_deref() == Some("source.organizeImports")),
        "{organize:?}"
    );
    let organized = match organize.first().and_then(|a| a.edit.as_ref()) {
        Some(edit) => match edit.changes.as_slice() {
            [FileChange::Edit { edits, .. }] => apply_text_edits(main, edits).unwrap(),
            other => panic!("unexpected organize imports edit {other:?}"),
        },
        None => panic!("organize imports offers no edit: {organize:?}"),
    };
    assert!(organized.contains("\"fmt\""), "{organized}");

    let symbols = futures_lite_block_on(client.document_symbols(&main_file)).unwrap();
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["total", "main"]);
    assert_eq!(
        symbols[0].range.start,
        Position {
            line: 4,
            character: 5
        }
    );

    let found = futures_lite_block_on(client.workspace_symbols("Square")).unwrap();
    let square = found
        .iter()
        .find(|s| s.name == "Square" && s.path == shapes_file)
        .unwrap_or_else(|| panic!("Square not found in {found:?}"));
    assert_eq!(square.range.start.line, 6);

    // From the interface method's call, the concrete method is the implementation.
    let call = Position {
        line: 7,
        character: 12,
    };
    let impls = futures_lite_block_on(client.implementation(&main_file, call)).unwrap();
    assert!(
        impls
            .iter()
            .any(|l| l.path == shapes_file && l.range.start.line == 10),
        "{impls:?}"
    );

    let at_total = Position {
        line: 4,
        character: 6,
    };
    let target = futures_lite_block_on(client.prepare_rename(&main_file, at_total)).unwrap();
    match target {
        Some(RenameTarget::Range(range, _)) => {
            assert_eq!((range.start.character, range.end.character), (5, 10))
        }
        other => panic!("unexpected prepareRename answer {other:?}"),
    }
    let keyword = Position {
        line: 0,
        character: 2,
    };
    assert!(
        !matches!(
            futures_lite_block_on(client.prepare_rename(&main_file, keyword)),
            Ok(Some(_))
        ),
        "a keyword cannot be renamed"
    );
    let edit = futures_lite_block_on(client.rename(&main_file, at_total, "sumAreas")).unwrap();
    let [
        FileChange::Edit {
            path,
            version,
            edits,
        },
    ] = edit.changes.as_slice()
    else {
        panic!("one file changes: {edit:?}");
    };
    assert_eq!(path, &main_file);
    assert_eq!(
        *version,
        Some(1),
        "the edit names the version it was made for"
    );
    let renamed = apply_text_edits(main, edits).unwrap();
    assert_eq!(renamed.matches("sumAreas(").count(), 2, "{renamed}");
    assert!(!renamed.contains("total("));

    // Renaming an exported method reaches into the closed file that declares it; gopls only
    // renames across packages once the file compiles.
    client.did_change(&main_file, 2, fixed.clone());
    let area = Position {
        line: fixed.lines().position(|l| l.contains("s.Area()")).unwrap() as u32,
        character: 12,
    };
    let edit = futures_lite_block_on(client.rename(&main_file, area, "Size")).unwrap();
    let files: Vec<_> = edit
        .changes
        .iter()
        .filter_map(|c| match c {
            FileChange::Edit { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert!(
        files.contains(&shapes_file) && files.contains(&main_file),
        "{edit:?}"
    );

    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gopls_lists_the_callers_and_callees_of_a_function() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let source = "package main\n\nfunc helper() int { return leaf() + leaf() }\n\nfunc leaf() int { return 1 }\n\nfunc main() { _ = helper() }\n";
    let Opened {
        client, dir, file, ..
    } = open_go("calls", source, serde_json::Value::Null);
    assert!(client.supports("/callHierarchyProvider"));
    let on_helper = Position {
        line: 2,
        character: 6,
    };
    let items = futures_lite_block_on(client.prepare_call_hierarchy(&file, on_helper)).unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    let helper = &items[0];
    assert_eq!(helper.name, "helper");
    assert_eq!(helper.path, file);
    assert_eq!(helper.selection.start.line, 2);

    let callers = futures_lite_block_on(client.incoming_calls(helper)).unwrap();
    assert_eq!(callers.len(), 1, "{callers:?}");
    assert_eq!(callers[0].item.name, "main");
    let sites: Vec<(u32, u32)> = callers[0]
        .ranges
        .iter()
        .map(|r| (r.start.line, r.start.character))
        .collect();
    assert_eq!(sites, [(6, 18)], "the call in main");

    let callees = futures_lite_block_on(client.outgoing_calls(helper)).unwrap();
    assert_eq!(callees.len(), 1, "{callees:?}");
    assert_eq!(callees[0].item.name, "leaf");
    assert_eq!(callees[0].ranges.len(), 2, "both calls, in helper's file");
    assert!(callees[0].ranges.iter().all(|r| r.start.line == 2));

    let nothing = Position {
        line: 1,
        character: 0,
    };
    assert!(
        futures_lite_block_on(client.prepare_call_hierarchy(&file, nothing))
            .unwrap_or_default()
            .is_empty()
    );
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gopls_grows_a_selection_through_the_syntax_around_each_cursor() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let source = "package main\n\nfunc main() {\n\tx := add(1, 2)\n\t_ = x\n}\n\nfunc add(a, b int) int { return a + b }\n";
    let Opened {
        client, dir, file, ..
    } = open_go("selection", source, serde_json::Value::Null);
    assert!(client.supports("/selectionRangeProvider"));
    let in_args = Position {
        line: 3,
        character: 10,
    };
    let in_return = Position {
        line: 7,
        character: 34,
    };
    let chains =
        futures_lite_block_on(client.selection_ranges(&file, &[in_args, in_return])).unwrap();
    assert_eq!(chains.len(), 2, "one chain per cursor");
    for chain in &chains {
        assert!(chain.len() > 2, "{chain:?}");
        for pair in chain.windows(2) {
            assert!(
                pair[1].start <= pair[0].start && pair[0].end <= pair[1].end,
                "each range holds the one before: {chain:?}"
            );
        }
    }
    let first = chains[0][0];
    assert_eq!(
        (first.start.line, first.start.character),
        (3, 10),
        "the literal 1"
    );
    assert!(
        chains[0].iter().any(|r| r.start
            == Position {
                line: 3,
                character: 6
            }
            && r.end.character == 15),
        "the call add(1, 2): {:?}",
        chains[0]
    );
    // gopls does not take part in renames; the tree renames files without asking it.
    assert!(!client.wants_rename(true, &file, false));
    assert!(!client.supports("/documentRangeFormattingProvider"));
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gopls_lists_the_types_implementing_an_interface_and_the_interfaces_a_type_implements() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let source = "package main\n\ntype Shape interface{ Area() int }\n\ntype Square struct{ n int }\n\nfunc (s Square) Area() int { return s.n * s.n }\n\nfunc main() { var _ Shape = Square{} }\n";
    let Opened {
        client, dir, file, ..
    } = open_go("types", source, serde_json::Value::Null);
    assert!(client.supports("/typeHierarchyProvider"));
    let on_shape = Position {
        line: 2,
        character: 6,
    };
    let items = futures_lite_block_on(client.prepare_type_hierarchy(&file, on_shape)).unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].name, "Shape");
    let subtypes = futures_lite_block_on(client.type_hierarchy(&items[0], false)).unwrap();
    assert!(
        subtypes
            .iter()
            .any(|t| t.name == "Square" && t.path == file),
        "{subtypes:?}"
    );
    let square = subtypes.iter().find(|t| t.name == "Square").unwrap();
    assert_eq!(square.selection.start.line, 4);
    let supertypes = futures_lite_block_on(client.type_hierarchy(square, true)).unwrap();
    assert!(
        supertypes.iter().any(|t| t.name == "Shape"),
        "{supertypes:?}"
    );
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A minimal executor: the definition future only waits on a channel the reader thread fills.
/// Opens `source` as main.go in a new module and waits for gopls to have read it.
fn open_go(name: &str, source: &str, config: serde_json::Value) -> Opened {
    let dir = std::env::temp_dir().join(format!("athena-lsp-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("go.mod"), "module example.com/probe\n\ngo 1.21\n").unwrap();
    let file = dir.join("main.go");
    std::fs::write(&file, source).unwrap();
    let config = std::sync::Arc::new(std::sync::RwLock::new(config));
    let (client, events) = Client::start_with(ServerKind::Go, dir.clone(), config.clone());
    client.did_open(&file, "go", 1, source.into());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match next_event(&events, deadline).expect("no diagnostics from gopls in time") {
            Event::Diagnostics { path, .. } if path == file => break,
            Event::Stopped(why) => panic!("gopls stopped: {why}"),
            _ => {}
        }
    }
    Opened {
        client,
        config,
        dir,
        file,
    }
}

struct Opened {
    client: Client,
    config: athena_lsp::Config,
    dir: std::path::PathBuf,
    file: std::path::PathBuf,
}

#[test]
fn gopls_highlights_where_the_symbol_under_the_cursor_is_read_and_written() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let source =
        "package main\n\nfunc main() {\n\tcount := 1\n\tcount = count + 1\n\t_ = count\n}\n";
    let Opened {
        client, dir, file, ..
    } = open_go("highlight", source, serde_json::Value::Null);
    let on_count = Position {
        line: 4,
        character: 2,
    };
    let mut found = futures_lite_block_on(client.document_highlights(&file, on_count)).unwrap();
    found.sort_by_key(|h| h.range.start);
    let lines: Vec<(u32, u32)> = found
        .iter()
        .map(|h| (h.range.start.line, h.range.start.character))
        .collect();
    assert_eq!(lines, [(3, 1), (4, 1), (4, 9), (5, 5)], "{found:?}");
    assert!(
        found
            .iter()
            .all(|h| h.range.end.character - h.range.start.character == 5)
    );
    let nothing = Position {
        line: 1,
        character: 0,
    };
    assert!(
        futures_lite_block_on(client.document_highlights(&file, nothing))
            .unwrap_or_default()
            .is_empty()
    );
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gopls_sends_the_inlay_hints_its_settings_ask_for() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let source = "package main\n\nfunc helper(n int) int { return n }\n\nfunc main() {\n\tx := helper(1)\n\t_ = x\n}\n";
    let hints = serde_json::json!({"hints": {"parameterNames": true, "assignVariableTypes": true}});
    let Opened {
        client, dir, file, ..
    } = open_go("inlay", source, hints.clone());
    let whole = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 8,
            character: 0,
        },
    };
    let mut found = futures_lite_block_on(client.inlay_hints(&file, whole)).unwrap();
    found.sort_by_key(|h| h.position);
    let shown: Vec<(u32, u32, &str, bool)> = found
        .iter()
        .map(|h| {
            (
                h.position.line,
                h.position.character,
                h.label.trim(),
                h.is_type,
            )
        })
        .collect();
    assert_eq!(
        shown,
        [(5, 2, "int", true), (5, 13, "n:", false)],
        "{found:?}"
    );
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);

    let Opened {
        client,
        config,
        dir,
        file,
        ..
    } = open_go("no-inlay", source, serde_json::Value::Null);
    let found = futures_lite_block_on(client.inlay_hints(&file, whole)).unwrap();
    assert!(
        found.is_empty(),
        "gopls shows no hints by default: {found:?}"
    );

    *config.write().unwrap() = hints.clone();
    client.did_change_configuration(hints);
    // gopls asks for its settings again and uses them from its next answer on.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut found = Vec::new();
    while found.len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        found = futures_lite_block_on(client.inlay_hints(&file, whole)).unwrap();
    }
    assert_eq!(
        found.len(),
        2,
        "settings changed while it runs apply: {found:?}"
    );
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gopls_classifies_names_with_semantic_tokens_and_answers_again_after_an_edit() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let source = "package main\n\nconst limit = 3\n\nfunc helper(n int) int { return n + limit }\n\nfunc main() { _ = helper(1) }\n";
    let Opened {
        client, dir, file, ..
    } = open_go(
        "semantic",
        source,
        serde_json::json!({"semanticTokens": true}),
    );
    let legend = client.semantic_legend().expect("gopls sends a legend");
    let tokens = |reply: SemanticReply, previous: &[u32]| -> (Option<String>, Vec<u32>) {
        match reply {
            SemanticReply::Full(t) => (t.result_id, t.data),
            SemanticReply::Delta { result_id, edits } => {
                (result_id, apply_semantic_edits(previous, &edits).unwrap())
            }
        }
    };
    let kind_at = |data: &[u32], line: u32, start: u32| -> Option<(String, u32)> {
        decode_semantic_tokens(data)
            .into_iter()
            .find(|t| t.line == line && t.start == start)
            .map(|t| (legend.types[t.kind as usize].clone(), t.modifiers))
    };
    let first = futures_lite_block_on(client.semantic_tokens(&file, None))
        .unwrap()
        .parse()
        .expect("a readable reply");
    let (id, data) = tokens(first, &[]);
    assert_eq!(kind_at(&data, 4, 5).unwrap().0, "function", "helper");
    assert_eq!(kind_at(&data, 4, 12).unwrap().0, "parameter", "n");
    let (kind, modifiers) = kind_at(&data, 4, 36).expect("limit is classified");
    assert_eq!(kind, "variable");
    assert_ne!(
        modifiers & legend.modifier_bit("readonly"),
        0,
        "a constant is read-only"
    );

    let edited = source.replace("_ = helper(1)", "x := helper(2); _ = x");
    client.did_change(&file, 2, edited);
    let again = futures_lite_block_on(client.semantic_tokens(&file, id.as_deref()))
        .unwrap()
        .parse()
        .expect("a readable reply");
    let (_, data) = tokens(again, &data);
    assert_eq!(kind_at(&data, 6, 14).unwrap().0, "variable", "x");
    assert_eq!(kind_at(&data, 6, 19).unwrap().0, "function", "helper");
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gopls_offers_code_lenses_that_run_as_commands() {
    if athena_lsp::find_program("gopls").is_none() {
        eprintln!("gopls not installed; skipping");
        return;
    }
    let source = "package main\n\n//go:generate echo hello\n\nfunc main() {}\n";
    let Opened {
        client, dir, file, ..
    } = open_go("codelens", source, serde_json::Value::Null);
    let lenses = futures_lite_block_on(client.code_lens(&file)).unwrap();
    let mut lenses: Vec<CodeLens> = lenses
        .iter()
        .map(|l| futures_lite_block_on(client.resolve_code_lens(l)).unwrap())
        .collect();
    lenses.sort_by_key(|l| l.range.start);
    let shown: Vec<(u32, String, String)> = lenses
        .iter()
        .map(|l| {
            let c = l.command.as_ref().expect("resolved");
            (l.range.start.line, c.title.clone(), c.command.clone())
        })
        .collect();
    assert!(
        shown.iter().any(|(line, title, command)| *line == 2
            && title.contains("go generate")
            && command == "gopls.generate"),
        "{shown:?}"
    );
    let generate = lenses
        .iter()
        .filter_map(|l| l.command.as_ref())
        .find(|c| c.command == "gopls.generate")
        .unwrap();
    assert!(
        futures_lite_block_on(client.execute_command(generate)).is_ok(),
        "gopls runs its own lens command"
    );
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);

    let Opened { client, dir, .. } = open_go(
        "codelens-test",
        "package main\n",
        serde_json::json!({"codelenses": {"test": true}}),
    );
    let tests = "package main\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {}\n";
    let file = dir.join("main_test.go");
    std::fs::write(&file, tests).unwrap();
    client.did_open(&file, "go", 1, tests.into());
    let lenses = futures_lite_block_on(client.code_lens(&file)).unwrap();
    assert!(
        lenses.iter().any(|l| l.range.start.line == 4
            && l.command
                .as_ref()
                .is_some_and(|c| c.command == "gopls.run_tests")),
        "the test lens follows gopls's codelenses setting: {lenses:?}"
    );
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, Wake, Waker};
    struct Thread(std::thread::Thread);
    impl Wake for Thread {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(std::sync::Arc::new(Thread(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park_timeout(Duration::from_millis(100));
    }
}
