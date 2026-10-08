//! Prints what the watcher sees: `cargo run -p athena-containers --example probe`.
fn main() {
    let (tx, rx) = async_channel::unbounded();
    let handle = athena_containers::start(tx);
    let mut watched = false;
    while let Ok(update) = rx.recv_blocking() {
        match update {
            athena_containers::Update::Containers(list) => {
                for c in &list {
                    println!(
                        "{:<28} {:<10} {:<12} {:?}",
                        c.name,
                        c.state,
                        c.project.as_deref().unwrap_or("-"),
                        c.ports
                    );
                }
                if !watched {
                    let running: Vec<String> = list
                        .iter()
                        .filter(|c| c.state == "running")
                        .map(|c| c.id.clone())
                        .collect();
                    if let Some(first) = running.first() {
                        handle.follow_logs(Some(first.clone()));
                    }
                    handle.watch_stats(running);
                    watched = true;
                }
            }
            athena_containers::Update::Stats(s) => {
                println!(
                    "stats for {} containers, e.g. {:?}",
                    s.len(),
                    s.values().next()
                );
                break;
            }
            athena_containers::Update::Logs { lines, .. } => println!("log lines: {}", lines.len()),
            athena_containers::Update::Unreachable(t) => {
                println!("unreachable: {t}");
                break;
            }
        }
    }
}
