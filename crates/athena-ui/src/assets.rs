use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

macro_rules! assets {
    ($($path:literal),* $(,)?) => {
        const FILES: &[(&str, &[u8])] = &[$(($path, include_bytes!(concat!("../assets/", $path)))),*];
    };
}

assets!(
    "brand/tlsc.svg",
    "icons/audio.svg",
    "icons/c-sharp.svg",
    "icons/c.svg",
    "icons/clojure.svg",
    "icons/config.svg",
    "icons/cpp.svg",
    "icons/css.svg",
    "icons/csv.svg",
    "icons/dart.svg",
    "icons/db.svg",
    "icons/default.svg",
    "icons/docker.svg",
    "icons/editorconfig.svg",
    "icons/elixir.svg",
    "icons/eslint.svg",
    "icons/folder.svg",
    "icons/font.svg",
    "icons/git_ignore.svg",
    "icons/git.svg",
    "icons/go.svg",
    "icons/graphql.svg",
    "icons/haskell.svg",
    "icons/html.svg",
    "icons/image.svg",
    "icons/java.svg",
    "icons/javascript.svg",
    "icons/json.svg",
    "icons/kotlin.svg",
    "icons/less.svg",
    "icons/license.svg",
    "icons/lock.svg",
    "icons/lua.svg",
    "icons/makefile.svg",
    "icons/markdown.svg",
    "icons/npm.svg",
    "icons/pdf.svg",
    "icons/php.svg",
    "icons/prisma.svg",
    "icons/python.svg",
    "icons/react.svg",
    "icons/ruby.svg",
    "icons/rust.svg",
    "icons/sass.svg",
    "icons/scala.svg",
    "icons/settings.svg",
    "icons/shell.svg",
    "icons/svelte.svg",
    "icons/svg.svg",
    "icons/swift.svg",
    "icons/terraform.svg",
    "icons/tex.svg",
    "icons/tsconfig.svg",
    "icons/typescript.svg",
    "icons/video.svg",
    "icons/vite.svg",
    "icons/vue.svg",
    "icons/wasm.svg",
    "icons/xml.svg",
    "icons/yarn.svg",
    "icons/yml.svg",
    "icons/zig.svg",
    "icons/zip.svg",
);

/// The `'static` name of a bundled asset.
pub(crate) fn bundled(path: &str) -> Option<&'static str> {
    FILES.iter().find(|(p, _)| *p == path).map(|(p, _)| *p)
}

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
