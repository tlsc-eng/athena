//! macOS banners and the Dock badge. Only active inside a bundle: UserNotifications needs a bundle
//! identifier, so `cargo run` builds keep to in-app notices.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Notification ids from banners the user clicked, read by the shell.
static CLICKS: OnceLock<async_channel::Sender<u64>> = OnceLock::new();

/// Whether macOS last said Athena may show banners; until it answers, assume so.
static ALLOWED: AtomicBool = AtomicBool::new(true);

#[cfg(target_os = "macos")]
mod mac {
    use block2::{DynBlock, RcBlock};
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{AllocAnyThread, define_class, msg_send};
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{MainThreadMarker, NSArray, NSBundle, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };

    use super::{ALLOWED, CLICKS};
    use std::sync::atomic::Ordering;

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "AthenaNotificationDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                // Athena only posts when you are not looking, so show it even if it is frontmost.
                handler.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                handler: &DynBlock<dyn Fn()>,
            ) {
                let id = response.notification().request().identifier().to_string();
                if let (Ok(id), Some(tx)) = (id.parse::<u64>(), CLICKS.get()) {
                    let _ = tx.try_send(id);
                }
                handler.call(());
            }
        }
    );

    impl Delegate {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    pub fn bundled() -> bool {
        NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    pub fn init() {
        if !bundled() {
            return;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate = Delegate::new();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // The center holds its delegate weakly; this one lives for the whole process.
        std::mem::forget(delegate);
        ask();
    }

    /// Asks for permission, which prompts only the first time and otherwise reports the setting.
    pub fn ask() {
        if !bundled() {
            return;
        }
        let done = RcBlock::new(|granted: Bool, error: *mut NSError| {
            let was = ALLOWED.swap(granted.as_bool(), Ordering::Relaxed);
            if was && !granted.as_bool() {
                tracing::warn!("notifications not allowed: {}", describe(error));
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert
                    | UNAuthorizationOptions::Sound
                    | UNAuthorizationOptions::Badge,
                &done,
            );
    }

    pub fn post(id: u64, title: &str, body: &str) {
        if !bundled() {
            return;
        }
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&id.to_string()),
            &content,
            None,
        );
        let done = RcBlock::new(|error: *mut NSError| {
            if !error.is_null() {
                tracing::warn!("notification not posted: {}", describe(error));
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, Some(&done));
    }

    pub fn withdraw(ids: &[u64]) {
        if !bundled() {
            return;
        }
        let ids: Vec<Retained<NSString>> = ids
            .iter()
            .map(|id| NSString::from_str(&id.to_string()))
            .collect();
        UNUserNotificationCenter::currentNotificationCenter()
            .removeDeliveredNotificationsWithIdentifiers(&NSArray::from_retained_slice(&ids));
    }

    fn describe(error: *mut NSError) -> String {
        // SAFETY: the completion handlers pass a valid NSError or null.
        match unsafe { error.as_ref() } {
            Some(error) => error.localizedDescription().to_string(),
            None => "no error given".into(),
        }
    }

    pub fn set_badge(count: usize) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
        NSApplication::sharedApplication(mtm)
            .dockTile()
            .setBadgeLabel(label.as_deref());
    }
}

/// Asks for permission to notify (first launch only) and routes banner clicks to `clicks`.
pub fn init(clicks: async_channel::Sender<u64>) {
    let _ = CLICKS.set(clicks);
    #[cfg(target_os = "macos")]
    mac::init();
}

/// Rereads whether banners are allowed, which System Settings can change at any time.
pub fn recheck() {
    #[cfg(target_os = "macos")]
    mac::ask();
}

/// Whether `post` can show a banner: not outside the app bundle, nor once banners are denied.
pub fn available() -> bool {
    #[cfg(target_os = "macos")]
    return mac::bundled() && ALLOWED.load(Ordering::Relaxed);
    #[cfg(not(target_os = "macos"))]
    false
}

pub fn post(id: u64, title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    mac::post(id, title, body);
}

/// Takes banners for notifications already dealt with out of Notification Center.
pub fn withdraw(ids: &[u64]) {
    #[cfg(target_os = "macos")]
    mac::withdraw(ids);
}

pub fn set_badge(count: usize) {
    #[cfg(target_os = "macos")]
    mac::set_badge(count);
}
