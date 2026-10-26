//! Original Rowboat mark, embedded alongside the complete built-in icon catalog.
use gpui_kit::{AssetSource, SharedString};
use std::borrow::Cow;

pub const LOGO_PATH: &str = "rowboat/rowboat.svg";

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if path == LOGO_PATH {
            return Ok(Some(Cow::Borrowed(include_bytes!("../assets/rowboat.svg"))));
        }
        gpui_kit::assets::AllAssets.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut assets = gpui_kit::assets::AllAssets.list(path)?;
        if LOGO_PATH.starts_with(path) {
            assets.push(LOGO_PATH.into());
        }
        Ok(assets)
    }
}
