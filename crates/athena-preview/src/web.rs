use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use gpui::{Bounds, Pixels, Window};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{NSResponder, NSView, NSWindowOrderingMode};
use objc2_foundation::{
    NSError, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKUIDelegate,
    WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// What the page did, reported from WebKit's delegate callbacks.
pub(crate) enum WebEvent {
    Committed(String),
    Finished,
    Failed(String),
}

pub(crate) struct Ivars {
    events: async_channel::Sender<WebEvent>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AthenaPreviewDelegate"]
    #[ivars = Ivars]
    pub(crate) struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl WKNavigationDelegate for Delegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_policy(
            &self,
            _web: &WKWebView,
            action: &WKNavigationAction,
            handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            // SAFETY: a navigation action always carries a request.
            let url = unsafe { action.request().URL() };
            let policy = if url.is_some_and(|u| allowed(&url_string(&u))) {
                WKNavigationActionPolicy::Allow
            } else {
                WKNavigationActionPolicy::Cancel
            };
            handler.call((policy,));
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, web: &WKWebView, _navigation: Option<&WKNavigation>) {
            // SAFETY: reading the URL of a live web view on the main thread.
            if let Some(url) = unsafe { web.URL() } {
                let _ = self
                    .ivars()
                    .events
                    .try_send(WebEvent::Committed(url_string(&url)));
            }
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _web: &WKWebView, _navigation: Option<&WKNavigation>) {
            let _ = self.ivars().events.try_send(WebEvent::Finished);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            _web: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.failed(error);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, _web: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            self.failed(error);
        }
    }

    // No methods: without `createWebView…`, target=_blank and window.open open nothing.
    unsafe impl WKUIDelegate for Delegate {}
);

impl Delegate {
    fn new(events: async_channel::Sender<WebEvent>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { events });
        // SAFETY: NSObject's init on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }

    fn failed(&self, error: &NSError) {
        // NSURLErrorCancelled: refused by the policy above, or replaced by a newer load.
        if error.code() == -999 {
            return;
        }
        let message = error.localizedDescription().to_string();
        let _ = self.ivars().events.try_send(WebEvent::Failed(message));
    }
}

fn url_string(url: &NSURL) -> String {
    url.absoluteString()
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Pages may only go to http(s); file:, data: and custom schemes are refused.
pub(crate) fn allowed(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let scheme = lower.split_once(':').map_or("", |(s, _)| s);
    matches!(scheme, "http" | "https") || matches!(lower.as_str(), "about:blank" | "about:srcdoc")
}

thread_local! {
    /// One cookie jar per project, in memory only, so nothing survives a relaunch.
    static STORES: RefCell<HashMap<PathBuf, Retained<WKWebsiteDataStore>>> = RefCell::default();
}

fn store_for(root: &Path, mtm: MainThreadMarker) -> Retained<WKWebsiteDataStore> {
    STORES.with_borrow_mut(|stores| {
        stores
            .entry(root.to_path_buf())
            // SAFETY: called on the main thread, as the marker proves.
            .or_insert_with(|| unsafe { WKWebsiteDataStore::nonPersistentDataStore(mtm) })
            .clone()
    })
}

/// gpui's Metal-backed view; web views go beside it, since it draws over its own subviews.
fn host_view(window: &Window) -> Option<&NSView> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    let view: NonNull<NSView> = handle.ns_view.cast();
    // SAFETY: gpui's view lives as long as the window, which outlives this borrow.
    Some(unsafe { view.as_ref() })
}

fn responder_in(responder: &NSResponder, view: &NSView) -> bool {
    responder
        .downcast_ref::<NSView>()
        .is_some_and(|v| v.isDescendantOf(view))
}

/// A WKWebView laid over part of a gpui window.
pub(crate) struct Web {
    view: Retained<WKWebView>,
    host: Retained<NSView>,
    _delegate: Retained<Delegate>,
}

impl Web {
    pub(crate) fn new(
        root: &Path,
        events: async_channel::Sender<WebEvent>,
        window: &Window,
    ) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let host = host_view(window)?;
        let delegate = Delegate::new(events, mtm);
        // SAFETY: WebKit objects created and configured on the main thread.
        let view = unsafe {
            let config = WKWebViewConfiguration::new(mtm);
            config.setWebsiteDataStore(&store_for(root, mtm));
            let view = WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                NSRect::ZERO,
                &config,
            );
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            view.setUIDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            view.setInspectable(cfg!(debug_assertions));
            view
        };
        view.setHidden(true);
        // SAFETY: reading the view hierarchy on the main thread.
        let parent = unsafe { host.superview() }?;
        parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Above, Some(host));
        Some(Self {
            view,
            host: host.retain(),
            _delegate: delegate,
        })
    }

    pub(crate) fn load(&self, url: &str) {
        let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) else {
            return;
        };
        let request = NSURLRequest::requestWithURL(&url);
        // SAFETY: loading into a live web view on the main thread.
        unsafe { self.view.loadRequest(&request) };
    }

    pub(crate) fn reload(&self) {
        // SAFETY: as above.
        unsafe { self.view.reload() };
    }

    pub(crate) fn back(&self) {
        // SAFETY: as above.
        unsafe { self.view.goBack() };
    }

    pub(crate) fn forward(&self) {
        // SAFETY: as above.
        unsafe { self.view.goForward() };
    }

    /// Moves the view over `bounds`, given in gpui's top-left window coordinates.
    pub(crate) fn place(&self, bounds: Bounds<Pixels>, window: &Window) {
        let Some(host) = host_view(window) else {
            return;
        };
        // gpui's view is not flipped, so AppKit measures y up from the bottom.
        let height = host.bounds().size.height;
        let (x, y): (f32, f32) = (bounds.origin.x.into(), bounds.origin.y.into());
        let (w, h): (f32, f32) = (bounds.size.width.into(), bounds.size.height.into());
        let frame = NSRect::new(
            NSPoint::new(x as f64, height - (y + h) as f64),
            NSSize::new(w as f64, h as f64),
        );
        if self.view.frame() != frame {
            self.view.setFrame(frame);
        }
    }

    pub(crate) fn set_hidden(&self, hidden: bool) {
        if self.view.isHidden() != hidden {
            self.view.setHidden(hidden);
        }
        if hidden {
            self.resign_key();
        }
    }

    pub(crate) fn take_key(&self) {
        if let Some(window) = self.view.window() {
            window.makeFirstResponder(Some(&self.view));
        }
    }

    /// Hands keystrokes back to gpui if the page has them.
    pub(crate) fn resign_key(&self) {
        let Some(window) = self.view.window() else {
            return;
        };
        if window
            .firstResponder()
            .is_some_and(|r| responder_in(&r, &self.view))
        {
            window.makeFirstResponder(Some(&self.host));
        }
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        self.resign_key();
        self.view.removeFromSuperview();
    }
}

/// Gives keystrokes back to gpui if any web view in `window` holds them.
pub fn restore_key_focus(window: &Window) {
    let Some(host) = host_view(window) else {
        return;
    };
    let Some(ns_window) = host.window() else {
        return;
    };
    let foreign = ns_window
        .firstResponder()
        .is_some_and(|r| r.downcast_ref::<NSView>().is_some_and(|v| v != host));
    if foreign {
        ns_window.makeFirstResponder(Some(host));
    }
}

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn only_web_schemes_are_allowed() {
        assert!(allowed("http://localhost:3000/"));
        assert!(allowed("HTTPS://example.com"));
        assert!(allowed("about:blank"));
        assert!(!allowed("file:///etc/passwd"));
        assert!(!allowed("javascript:alert(1)"));
        assert!(!allowed("data:text/html,hi"));
        assert!(!allowed("vscode://open"));
        assert!(!allowed("about:config"));
    }
}
