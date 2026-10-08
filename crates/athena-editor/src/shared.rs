use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use anyhow::Result;
use gpui::{App, AppContext, Entity, Global};

use crate::buffer::Buffer;

/// One file's text, held by every editor tab showing that file.
pub(crate) struct SharedBuffer {
    pub buffer: RefCell<Buffer>,
    /// Notified after each change, so the other views catch up and redraw.
    pub signal: Entity<Signal>,
}

pub(crate) struct Signal;

impl SharedBuffer {
    fn new(buffer: Buffer, cx: &mut App) -> Rc<Self> {
        Rc::new(Self {
            buffer: RefCell::new(buffer),
            signal: cx.new(|_| Signal),
        })
    }

    pub fn changed(&self, cx: &mut App) {
        self.signal.update(cx, |_, cx| cx.notify());
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
