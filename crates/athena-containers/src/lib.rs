//! Read-only view of the local Docker engine (Rancher Desktop or Docker Desktop).
//!
//! bollard needs tokio, which the window does not run, so the client lives on its own thread with
//! a small runtime and talks to the UI over channels. Only ping, list, events, stats and logs are
//! ever called: Athena never starts, stops or removes anything.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use bollard::models::{ContainerStatsResponse, ContainerSummary};
use bollard::query_parameters::{
    EventsOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder, StatsOptionsBuilder,
};
use bollard::{API_DEFAULT_VERSION, Docker};
use futures::StreamExt;
use tokio::sync::mpsc;

const RETRY: Duration = Duration::from_secs(10);
const STATS_EVERY: Duration = Duration::from_secs(2);
const LOG_TAIL: &str = "500";

#[derive(Clone, Debug, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    /// `running`, `exited`, `restarting`, `paused`, ...
    pub state: String,
    /// Docker's own summary, such as "Up 3 hours".
    pub status: String,
    pub ports: Vec<String>,
    /// Docker Compose project, which is how containers are grouped.
    pub project: Option<String>,
    pub service: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    pub cpu_percent: f32,
    pub memory_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// No engine reachable; says which sockets were tried.
    Unreachable(String),
    Containers(Vec<Container>),
    Stats(HashMap<String, Stats>),
    /// New log lines for the followed container (appended to what was sent before).
    Logs {
        id: String,
        lines: Vec<String>,
    },
}

pub enum Command {
    /// Containers whose CPU and memory are on screen; others are not polled.
    WatchStats(Vec<String>),
    /// Start following one container's logs, or stop with `None`.
    FollowLogs(Option<String>),
}

#[derive(Clone)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
}

impl Handle {
    pub fn watch_stats(&self, ids: Vec<String>) {
        let _ = self.commands.send(Command::WatchStats(ids));
    }

    pub fn follow_logs(&self, id: Option<String>) {
        let _ = self.commands.send(Command::FollowLogs(id));
    }
}

/// Candidate engine sockets: $DOCKER_HOST (unix only), Rancher Desktop, then the system default.
fn sockets() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(host) = std::env::var("DOCKER_HOST")
        && let Some(path) = host.strip_prefix("unix://")
    {
        out.push(PathBuf::from(path));
    }
    if let Some(home) = std::env::home_dir() {
        out.push(home.join(".rd/docker.sock"));
        out.push(home.join(".docker/run/docker.sock"));
    }
    out.push(PathBuf::from("/var/run/docker.sock"));
    out
}

async fn connect() -> Result<Docker, String> {
    let candidates = sockets();
    for path in candidates.iter().filter(|p| p.exists()) {
        let Ok(docker) = Docker::connect_with_unix(&path.to_string_lossy(), 5, API_DEFAULT_VERSION)
        else {
            continue;
        };
        if tokio::time::timeout(Duration::from_secs(2), docker.ping())
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return Ok(docker);
        }
    }
    let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    Err(tried.join(", "))
}

fn summarize(c: ContainerSummary) -> Container {
    let labels = c.labels.unwrap_or_default();
    let mut ports: Vec<String> = c
        .ports
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            p.public_port
                .map(|public| format!("{public}:{}", p.private_port))
        })
        .collect();
    ports.sort();
    ports.dedup();
    Container {
        id: c.id.unwrap_or_default(),
        name: c
            .names
            .and_then(|n| n.into_iter().next())
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string(),
        image: c.image.unwrap_or_default(),
        state: c.state.map(|s| s.to_string()).unwrap_or_default(),
        status: c.status.unwrap_or_default(),
        ports,
        project: labels.get("com.docker.compose.project").cloned(),
        service: labels.get("com.docker.compose.service").cloned(),
    }
}

/// CPU as a share of one core times the cores in use, the way `docker stats` reports it.
pub fn cpu_percent(s: &ContainerStatsResponse) -> f32 {
    let (Some(cpu), Some(pre)) = (&s.cpu_stats, &s.precpu_stats) else {
        return 0.;
    };
    let total = |c: &bollard::models::ContainerCpuStats| {
        c.cpu_usage
            .as_ref()
            .and_then(|u| u.total_usage)
            .unwrap_or(0)
    };
    let delta = total(cpu).saturating_sub(total(pre)) as f64;
    let system = cpu
        .system_cpu_usage
        .unwrap_or(0)
        .saturating_sub(pre.system_cpu_usage.unwrap_or(0)) as f64;
    let cores = cpu.online_cpus.unwrap_or(1).max(1) as f64;
    if system <= 0. {
        0.
    } else {
        (delta / system * cores * 100.) as f32
    }
}

/// Memory in use without reclaimable page cache, matching `docker stats`.
fn memory_in_use(m: &bollard::models::ContainerMemoryStats) -> u64 {
    let usage = m.usage.unwrap_or(0);
    let cache = m.stats.as_ref().and_then(|s| {
        s.get("inactive_file")
            .or_else(|| s.get("total_inactive_file"))
            .copied()
    });
    usage.saturating_sub(cache.unwrap_or(0))
}

async fn list(docker: &Docker) -> Result<Vec<Container>, bollard::errors::Error> {
    let options = ListContainersOptionsBuilder::new().all(true).build();
    let mut out: Vec<Container> = docker
        .list_containers(Some(options))
        .await?
        .into_iter()
        .map(summarize)
        .collect();
    out.sort_by(|a, b| a.project.cmp(&b.project).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

/// Starts the engine watcher on its own thread.
pub fn start(updates: async_channel::Sender<Update>) -> Handle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _ = std::thread::Builder::new()
        .name("containers".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let mut watched: Vec<String> = Vec::new();
                let mut following: Option<String> = None;
                loop {
                    match connect().await {
                        Ok(docker) => {
                            serve(&docker, &updates, &mut rx, &mut watched, &mut following).await
                        }
                        Err(tried) => {
                            if updates.send(Update::Unreachable(tried)).await.is_err() {
                                return;
                            }
                        }
                    }
                    // Keep taking commands while waiting to retry, so state is current on reconnect.
                    let deadline = tokio::time::sleep(RETRY);
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => break,
                            cmd = rx.recv() => match cmd {
                                Some(Command::WatchStats(ids)) => watched = ids,
                                Some(Command::FollowLogs(id)) => following = id,
                                None => return,
                            },
                        }
                    }
                }
            });
        });
    Handle { commands: tx }
}

/// Runs until the engine goes away.
async fn serve(
    docker: &Docker,
    updates: &async_channel::Sender<Update>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    watched: &mut Vec<String>,
    following: &mut Option<String>,
) {
    let Ok(containers) = list(docker).await else {
        return;
    };
    if updates.send(Update::Containers(containers)).await.is_err() {
        return;
    }
    let mut events = docker.events(Some(EventsOptionsBuilder::new().build()));
    let mut tick = tokio::time::interval(STATS_EVERY);
    let mut logs = following
        .clone()
        .map(|id| follow(docker.clone(), id, updates.clone()));
    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(e)) if e.typ.as_ref().is_some_and(|t| t.to_string() == "container") => {
                    match list(docker).await {
                        Ok(c) => { let _ = updates.send(Update::Containers(c)).await; }
                        Err(_) => return,
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return,
            },
            _ = tick.tick() => {
                if watched.is_empty() {
                    continue;
                }
                // Each sample takes about a second inside Docker, so ask for all of them at once.
                let samples = futures::future::join_all(watched.iter().map(|id| {
                    let options = StatsOptionsBuilder::new().stream(false).build();
                    let mut stream = docker.stats(id, Some(options));
                    async move { (id.clone(), stream.next().await) }
                }))
                .await;
                let stats: HashMap<String, Stats> = samples
                    .into_iter()
                    .filter_map(|(id, sample)| {
                        let s = sample?.ok()?;
                        let memory = s.memory_stats.as_ref().map_or(0, memory_in_use);
                        Some((id, Stats { cpu_percent: cpu_percent(&s), memory_bytes: memory }))
                    })
                    .collect();
                let _ = updates.send(Update::Stats(stats)).await;
            }
            cmd = commands.recv() => match cmd {
                Some(Command::WatchStats(ids)) => *watched = ids,
                Some(Command::FollowLogs(id)) => {
                    if let Some(task) = logs.take() { task.abort(); }
                    *following = id.clone();
                    logs = id.map(|id| follow(docker.clone(), id, updates.clone()));
                }
                None => return,
            },
        }
    }
}

fn follow(
    docker: Docker,
    id: String,
    updates: async_channel::Sender<Update>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let options = LogsOptionsBuilder::new()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .tail(LOG_TAIL)
            .build();
        let mut stream = docker.logs(&id, Some(options)).ready_chunks(512);
        while let Some(chunks) = stream.next().await {
            let lines: Vec<String> = chunks
                .into_iter()
                .filter_map(Result::ok)
                .flat_map(|chunk| {
                    let text = chunk.to_string();
                    text.lines()
                        .map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>())
                        .collect::<Vec<_>>()
                })
                .collect();
            if updates
                .send(Update::Logs {
                    id: id.clone(),
                    lines,
                })
                .await
                .is_err()
            {
                return;
            }
        }
    })
}
