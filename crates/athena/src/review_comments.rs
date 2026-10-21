//! Comments the user leaves on lines of a Claude session's changes, kept per session beside its
//! snapshots until they are sent to the session.

use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::snapshots;

/// How far from its old place a comment's anchor is looked for after the file changed.
const SEARCH_LINES: u32 = 200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// Unique within its session, and never reused after a delete.
    pub id: u64,
    pub session: String,
    pub path: PathBuf,
    /// One-based and inclusive.
    pub lines: RangeInclusive<u32>,
    /// The commented lines as they were when the comment was made, joined with `\n`.
    pub anchor: String,
    pub body: String,
    pub sent: bool,
    /// Unix seconds.
    pub created: u64,
}

/// Where a comment shows in the file as it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub lines: RangeInclusive<u32>,
    /// The anchor is gone from the file; `lines` is the old place.
    pub stale: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionFile {
    next_id: u64,
    comments: Vec<ReviewComment>,
}

fn file_of(store: &Path, session: &str) -> Result<PathBuf> {
    if !snapshots::valid_session(session) {
        bail!("bad session id");
    }
    Ok(store
        .join(snapshots::COMMENTS_DIR)
        .join(format!("{session}.json")))
}

fn read(store: &Path, session: &str) -> Result<SessionFile> {
    let file = file_of(store, session)?;
    match fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| file.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SessionFile::default()),
        Err(e) => Err(e).with_context(|| file.display().to_string()),
    }
}

fn write(store: &Path, session: &str, data: &SessionFile) -> Result<()> {
    let file = file_of(store, session)?;
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(data)?)?;
    fs::rename(&tmp, &file)?;
    Ok(())
}

/// Every comment of `session`, sent or not.
pub fn load(store: &Path, session: &str) -> Result<Vec<ReviewComment>> {
    Ok(read(store, session)?.comments)
}

/// The commented lines of `text` as an anchor, or `None` when the file has fewer lines.
pub fn anchor_of(text: &str, lines: &RangeInclusive<u32>) -> Option<String> {
    let all: Vec<&str> = text.lines().collect();
    let first = lines.start().checked_sub(1)? as usize;
    Some(all.get(first..*lines.end() as usize)?.join("\n"))
}

pub fn add(
    store: &Path,
    session: &str,
    path: &Path,
    lines: RangeInclusive<u32>,
    anchor: String,
    body: String,
) -> Result<ReviewComment> {
    if !path.is_absolute() || *lines.start() == 0 || lines.start() > lines.end() {
        bail!("bad comment range");
    }
    let mut data = read(store, session)?;
    let comment = ReviewComment {
        id: data.next_id,
        session: session.to_string(),
        path: path.to_path_buf(),
        lines,
        anchor,
        body,
        sent: false,
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    };
    data.next_id += 1;
    data.comments.push(comment.clone());
    write(store, session, &data)?;
    Ok(comment)
}

/// False when the session has no comment `id`.
pub fn update_body(store: &Path, session: &str, id: u64, body: String) -> Result<bool> {
    let mut data = read(store, session)?;
    let Some(comment) = data.comments.iter_mut().find(|c| c.id == id) else {
        return Ok(false);
    };
    comment.body = body;
    write(store, session, &data)?;
    Ok(true)
}

/// False when the session has no comment `id`.
pub fn delete(store: &Path, session: &str, id: u64) -> Result<bool> {
    let mut data = read(store, session)?;
    let before = data.comments.len();
    data.comments.retain(|c| c.id != id);
    if data.comments.len() == before {
        return Ok(false);
    }
    write(store, session, &data)?;
    Ok(true)
}

pub fn for_file(store: &Path, session: &str, path: &Path) -> Result<Vec<ReviewComment>> {
    let mut found = load(store, session)?;
    found.retain(|c| c.path == path);
    Ok(found)
}

pub fn unsent(store: &Path, session: &str) -> Result<Vec<ReviewComment>> {
    let mut found = load(store, session)?;
    found.retain(|c| !c.sent);
    Ok(found)
}

#[allow(dead_code)] // Called by delivery to the session, which is not built yet.
pub fn mark_sent(store: &Path, session: &str, ids: &[u64]) -> Result<()> {
    let mut data = read(store, session)?;
    for comment in data.comments.iter_mut().filter(|c| ids.contains(&c.id)) {
        comment.sent = true;
    }
    write(store, session, &data)
}

/// Where `comment` belongs in `text`: its own lines while they still read as the anchor, else the
/// nearest place within [`SEARCH_LINES`] that does, else stale at its old lines.
pub fn place(text: &str, comment: &ReviewComment) -> Placement {
    let all: Vec<&str> = text.lines().collect();
    let anchor: Vec<&str> = comment.anchor.split('\n').collect();
    let len = anchor.len();
    let old = *comment.lines.start();
    let fits = |start: u32| {
        let from = start.saturating_sub(1) as usize;
        start >= 1 && all.get(from..from + len) == Some(anchor.as_slice())
    };
    if fits(old) {
        return Placement {
            lines: comment.lines.clone(),
            stale: false,
        };
    }
    // On a tie the earlier of two equally near blocks wins.
    let moved = (1..=SEARCH_LINES).find_map(|d| {
        [old.checked_sub(d), old.checked_add(d)]
            .into_iter()
            .flatten()
            .find(|&s| fits(s))
    });
    match moved {
        Some(start) => Placement {
            lines: start..=start + len as u32 - 1,
            stale: false,
        },
        None => Placement {
            lines: comment.lines.clone(),
            stale: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("athena-comments-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make(store: &Path, session: &str, path: &str, line: u32, body: &str) -> ReviewComment {
        add(
            store,
            session,
            Path::new(path),
            line..=line,
            format!("line {line}"),
            body.to_string(),
        )
        .unwrap()
    }

    fn comment(lines: RangeInclusive<u32>, anchor: &str) -> ReviewComment {
        ReviewComment {
            id: 0,
            session: "s-1".into(),
            path: "/p/a.rs".into(),
            lines,
            anchor: anchor.into(),
            body: String::new(),
            sent: false,
            created: 0,
        }
    }

    fn numbered(n: u32) -> Vec<String> {
        (1..=n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn comments_round_trip_through_the_session_file_without_leaving_a_temp_file() {
        let store = temp("round");
        let added = add(
            &store,
            "s-1",
            Path::new("/p/a.rs"),
            3..=4,
            "x\ny".into(),
            "why?".into(),
        )
        .unwrap();
        assert_eq!(load(&store, "s-1").unwrap(), vec![added]);
        let dir = store.join(snapshots::COMMENTS_DIR);
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["s-1.json"]);
        assert!(load(&store, "other").unwrap().is_empty());
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn a_corrupt_file_is_an_error_and_is_not_overwritten_by_an_add() {
        let store = temp("corrupt");
        let dir = store.join(snapshots::COMMENTS_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("s-1.json"), "{ not json").unwrap();
        assert!(load(&store, "s-1").is_err());
        assert!(
            add(
                &store,
                "s-1",
                Path::new("/p/a"),
                1..=1,
                "a".into(),
                "b".into()
            )
            .is_err()
        );
        assert_eq!(fs::read(dir.join("s-1.json")).unwrap(), b"{ not json");
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn a_session_id_cannot_leave_the_comments_folder_and_bad_ranges_are_refused() {
        let store = temp("bad");
        assert!(load(&store, "../x").is_err());
        let p = Path::new("/p/a");
        assert!(add(&store, "s-1", p, 0..=1, String::new(), String::new()).is_err());
        let backwards = std::hint::black_box(3)..=2;
        assert!(add(&store, "s-1", p, backwards, String::new(), String::new()).is_err());
        assert!(
            add(
                &store,
                "s-1",
                Path::new("a"),
                1..=1,
                String::new(),
                String::new()
            )
            .is_err()
        );
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn update_delete_unsent_and_mark_sent_work_on_one_comment_at_a_time() {
        let store = temp("crud");
        let a = make(&store, "s-1", "/p/a.rs", 1, "one");
        let b = make(&store, "s-1", "/p/b.rs", 2, "two");
        let c = make(&store, "s-1", "/p/a.rs", 3, "three");
        make(&store, "s-2", "/p/a.rs", 1, "elsewhere");

        assert!(update_body(&store, "s-1", b.id, "TWO".into()).unwrap());
        assert!(!update_body(&store, "s-1", 99, "x".into()).unwrap());
        let in_a: Vec<_> = for_file(&store, "s-1", Path::new("/p/a.rs"))
            .unwrap()
            .into_iter()
            .map(|c| c.body)
            .collect();
        assert_eq!(in_a, ["one", "three"]);

        mark_sent(&store, "s-1", &[a.id, b.id]).unwrap();
        let left: Vec<_> = unsent(&store, "s-1")
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(left, [c.id]);
        assert_eq!(load(&store, "s-1").unwrap().len(), 3, "sent ones are kept");
        assert_eq!(unsent(&store, "s-2").unwrap().len(), 1);

        assert!(delete(&store, "s-1", c.id).unwrap());
        assert!(!delete(&store, "s-1", c.id).unwrap());
        assert!(unsent(&store, "s-1").unwrap().is_empty());
        let again = make(&store, "s-1", "/p/a.rs", 9, "new");
        assert!(again.id > c.id, "a deleted id is not handed out again");
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn the_anchor_is_the_commented_lines_and_missing_lines_give_none() {
        let text = "a\nb\r\nc\n";
        assert_eq!(anchor_of(text, &(2..=3)).as_deref(), Some("b\nc"));
        assert_eq!(anchor_of(text, &(3..=4)), None);
        assert_eq!(anchor_of(text, &(0..=1)), None);
    }

    #[test]
    fn an_unchanged_file_keeps_the_range() {
        let text = numbered(20).join("\n");
        let c = comment(5..=6, "line 5\nline 6");
        assert_eq!(
            place(&text, &c),
            Placement {
                lines: 5..=6,
                stale: false
            }
        );
    }

    #[test]
    fn lines_pushed_down_five_are_followed() {
        let mut lines = numbered(20);
        for i in 0..5 {
            lines.insert(0, format!("new {i}"));
        }
        let c = comment(5..=6, "line 5\nline 6");
        assert_eq!(
            place(&lines.join("\n"), &c),
            Placement {
                lines: 10..=11,
                stale: false
            }
        );
    }

    #[test]
    fn a_removed_anchor_is_stale_at_its_old_lines() {
        let text = numbered(20).join("\n").replace("line 6", "edited");
        let c = comment(5..=6, "line 5\nline 6");
        assert_eq!(
            place(&text, &c),
            Placement {
                lines: 5..=6,
                stale: true
            }
        );
    }

    #[test]
    fn of_two_copies_of_the_anchor_the_nearer_one_wins() {
        let mut lines: Vec<String> = (1..=40).map(|i| format!("filler {i}")).collect();
        lines[4] = "dup".into();
        lines[30] = "dup".into();
        let text = lines.join("\n");
        // Commented at line 25 before the file changed: line 31 is 6 away, line 5 is 20 away.
        let c = comment(25..=25, "dup");
        assert_eq!(place(&text, &c).lines, 31..=31);
        let c = comment(10..=10, "dup");
        assert_eq!(place(&text, &c).lines, 5..=5);
    }

    #[test]
    fn an_anchor_beyond_the_search_window_is_stale() {
        let mut lines: Vec<String> = (1..=500).map(|i| format!("filler {i}")).collect();
        lines[400] = "far".into();
        let c = comment(10..=10, "far");
        assert!(place(&lines.join("\n"), &c).stale);
        let c = comment(250..=250, "far");
        assert_eq!(place(&lines.join("\n"), &c).lines, 401..=401);
    }
}
