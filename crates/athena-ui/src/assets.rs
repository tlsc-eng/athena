use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

const FILES: &[(&str, &[u8])] = &[("brand/tlsc.svg", include_bytes!("../assets/brand/tlsc.svg"))];

/// Compiled-in assets, so the binary needs no resource directory at runtime.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(FILES
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(FILES
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}
