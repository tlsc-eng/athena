use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use anyhow::Result;
use gpui::{App, AppContext, Entity, Global, WeakEntity};

use crate::buffer::Buffer;
use crate::recovery;
use crate::view::EditorView;

/// One file's text, held by every editor tab showing that file.
pub(crate) struct SharedBuffer {
    pub buffer: RefCell<Buffer>,
    /// Notified after each change, so the other views catch up and redraw.
    pub signal: Entity<Signal>,
    /// Notified when a background parse lands, so views redraw with its highlights.
    pub parsed: Entity<Signal>,
    /// The views that have shown this buffer, for an auto save to keep blanks on all their carets.
    pub views: RefCell<Vec<WeakEntity<EditorView>>>,
    recovery_id: u64,
}

pub(crate) struct Signal;

impl SharedBuffer {
    fn new(mut buffer: Buffer, cx: &mut App) -> Rc<Self> {
        buffer.parse_in_background();
        Rc::new(Self {
            buffer: RefCell::new(buffer),
            signal: cx.new(|_| Signal),
            parsed: cx.new(|_| Signal),
            views: RefCell::default(),
            recovery_id: recovery::next_id(),
        })
    }

    pub fn changed(self: &Rc<Self>, cx: &mut App) {
        self.signal.update(cx, |_, cx| cx.notify());
        self.parse_soon(cx);
    }

    /// Parses the text on the background executor if the tree lags it, then again for any edits
    /// made meanwhile; the UI keeps the edited old tree until then.
    fn parse_soon(self: &Rc<Self>, cx: &mut App) {
        let Some(job) = self.buffer.borrow_mut().start_parse() else {
            return;
        };
        let this = Rc::downgrade(self);
        cx.spawn(async move |cx| {
            let parsed = cx.background_spawn(async move { job.run() }).await;
            cx.update(|cx| {
                let Some(this) = this.upgrade() else {
                    return;
                };
                if this.buffer.borrow_mut().finish_parse(parsed) {
                    this.parsed.update(cx, |_, cx| cx.notify());
                }
                this.parse_soon(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Hands unsaved text to the recovery list, or takes it off once saved.
    pub fn note_recovery(&self) {
        let b = self.buffer.borrow();
        let dirty = b.path.as_deref().filter(|_| b.is_dirty());
        recovery::note(self.recovery_id, dirty.map(|path| (path, b.rope())));
    }
}

impl Drop for SharedBuffer {
    fn drop(&mut self) {
        recovery::note(self.recovery_id, None);
    }
}

/// Open buffers by canonical path; an entry dies with the last tab showing it.
#[derive(Default)]
struct BufferStore(HashMap<PathBuf, Weak<SharedBuffer>>);

impl Global for BufferStore {}

/// Symlinked paths (/tmp) and their targets share one buffer, as language servers see one file.
fn key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The buffer already open for `path`, else the file read from disk.
pub(crate) fn open(path: &Path, cx: &mut App) -> Result<Rc<SharedBuffer>> {
    let key = key(path);
    let store = cx.default_global::<BufferStore>();
    if let Some(shared) = store.0.get(&key).and_then(Weak::upgrade) {
        return Ok(shared);
    }
    let shared = SharedBuffer::new(Buffer::open(path)?, cx);
    register(&shared, path, cx);
    Ok(shared)
}

/// A new buffer for `path` holding `buffer`, replacing any earlier one in the store.
pub(crate) fn adopt(buffer: Buffer, path: &Path, cx: &mut App) -> Rc<SharedBuffer> {
    let shared = SharedBuffer::new(buffer, cx);
    register(&shared, path, cx);
    shared
}

/// Files `shared` under `path` alone, e.g. after it was saved there.
pub(crate) fn register(shared: &Rc<SharedBuffer>, path: &Path, cx: &mut App) {
    let store = cx.default_global::<BufferStore>();
    store
        .0
        .retain(|_, weak| weak.strong_count() > 0 && !std::ptr::eq(weak.as_ptr(), &**shared));
    store.0.insert(key(path), Rc::downgrade(shared));
}
