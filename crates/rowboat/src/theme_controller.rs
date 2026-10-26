//! App-local palette ownership. Disk reads run off-thread, independently of renders
//! and shell ticks. Late system observations always resolve against the saved choice.
use gpui_kit::{App, BorrowAppContext, Global, Task, WindowAppearance};
use gpui_omarchy::Theme;
use std::{io::Read, path::Path, time::Duration};

pub struct Palette {
    pub id: &'static str,
    pub name: &'static str,
    pub colors: &'static str,
}
macro_rules! palettes {
    ($(($id:literal, $name:literal)),* $(,)?) => {
        pub const PALETTES: &[Palette] = &[$(Palette {
            id: $id, name: $name,
            colors: include_str!(concat!("../assets/themes/", $id, ".toml")),
        }),*];
    }
}
palettes![
    ("catppuccin", "Catppuccin"),
    ("catppuccin-latte", "Catppuccin Latte"),
    ("ethereal", "Ethereal"),
    ("everforest", "Everforest"),
    ("flexoki-light", "Flexoki Light"),
    ("gruvbox", "Gruvbox"),
    ("hackerman", "Hackerman"),
    ("kanagawa", "Kanagawa"),
    ("last-horizon", "Last Horizon"),
    ("lumon", "Lumon"),
    ("lupine", "Lupine"),
    ("matte-black", "Matte Black"),
    ("miasma", "Miasma"),
    ("nord", "Nord"),
    ("osaka-jade", "Osaka Jade"),
    ("retro-82", "Retro 82"),
    ("ristretto", "Ristretto"),
    ("rose-pine", "Rose Pine"),
    ("solitude", "Solitude"),
    ("tokyo-night", "Tokyo Night"),
    ("vantablack", "Vantablack"),
    ("white", "White"),
];

pub fn embedded(id: &str) -> Option<Theme> {
    let palette = PALETTES.iter().find(|p| p.id == id)?;
    Theme::from_colors_toml(palette.name, palette.colors).ok()
}
fn native_theme(appearance: WindowAppearance) -> Theme {
    match appearance {
        WindowAppearance::Light | WindowAppearance::VibrantLight => Theme::flexoki_light(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark => Theme::tokyo_night(),
    }
}

pub(crate) struct Controller {
    choice: String,
    appearance: WindowAppearance,
    omarchy: Option<Theme>,
    resolved: Theme,
    source: &'static str,
    _watch: Option<Task<()>>,
}
impl Global for Controller {}
impl Controller {
    fn resolve(&mut self, cx: &mut App) {
        let (next, source) = if let Some(theme) = embedded(&self.choice) {
            (theme, "Embedded palette")
        } else if let Some(theme) = &self.omarchy {
            (theme.clone(), "System · Omarchy")
        } else {
            (native_theme(self.appearance), "System · desktop appearance")
        };
        let changed = next != self.resolved || source != self.source;
        self.source = source;
        if changed {
            self.resolved = next.clone();
            next.apply(cx);
        }
    }
}

/// Initialize state without launching I/O (also used by isolated GUI fixtures).
pub fn ensure(cx: &mut App) {
    if cx.try_global::<Controller>().is_some() {
        return;
    }
    let appearance = cx.window_appearance();
    let resolved = native_theme(appearance);
    // Theme::apply also stops gpui-omarchy's watcher. One controller owns colors.
    resolved.clone().apply(cx);
    cx.set_global(Controller {
        choice: "system".into(),
        appearance,
        omarchy: None,
        resolved,
        source: "System · desktop appearance",
        _watch: None,
    });
}

/// One serial background check per second also survives missing directories,
/// atomic symlink replacement, malformed palettes, and a later Omarchy install.
pub fn init(cx: &mut App) {
    ensure(cx);
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from);
    let watch = cx.spawn(async move |cx| {
        loop {
            let home = home.clone();
            let theme = cx
                .background_executor()
                .spawn(async move { home.as_deref().and_then(load_omarchy) })
                .await;
            cx.update(|cx| system_observed(theme, cx));
            cx.background_executor().timer(Duration::from_secs(1)).await;
        }
    });
    cx.global_mut::<Controller>()._watch = Some(watch);
}

fn bounded_text(path: &Path, limit: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut contents = String::new();
    file.take(limit + 1).read_to_string(&mut contents).ok()?;
    (contents.len() as u64 <= limit).then_some(contents)
}
pub(crate) fn load_omarchy(home: &Path) -> Option<Theme> {
    let current = home.join(".local/state/omarchy/current");
    let current = match std::fs::symlink_metadata(&current) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            home.join(".config/omarchy/current")
        }
        _ => current,
    };
    let colors = bounded_text(&current.join("theme/colors.toml"), 64 * 1024)?;
    let name = bounded_text(&current.join("theme.name"), 256)
        .filter(|name| !name.trim().is_empty() && !name.trim().chars().any(char::is_control))
        .unwrap_or_else(|| "Omarchy".into());
    Theme::from_colors_toml(name.trim(), &colors).ok()
}

pub fn set_choice(choice: &str, cx: &mut App) {
    ensure(cx);
    if cx.global::<Controller>().choice == choice {
        return;
    }
    cx.update_global::<Controller, _>(|s, cx| {
        s.choice = choice.into();
        s.resolve(cx);
        cx.refresh_windows();
    });
}
pub fn appearance_changed(appearance: WindowAppearance, cx: &mut App) {
    ensure(cx);
    cx.update_global::<Controller, _>(|s, cx| {
        s.appearance = appearance;
        s.resolve(cx);
    });
}
pub(crate) fn system_observed(theme: Option<Theme>, cx: &mut App) {
    cx.update_global::<Controller, _>(|s, cx| {
        // An observation never captures the selection from when its I/O started.
        if s.omarchy != theme {
            s.omarchy = theme;
            s.resolve(cx);
        }
    });
}
pub fn description(cx: &App) -> String {
    match cx.try_global::<Controller>() {
        Some(s) => format!(
            "Currently: {} — {}{}",
            s.resolved.name,
            s.source,
            if s.choice != "system" && !PALETTES.iter().any(|p| p.id == s.choice) {
                " (saved palette unavailable)"
            } else {
                ""
            }
        ),
        None => {
            "System follows Omarchy when available, otherwise your desktop's light/dark appearance."
                .into()
        }
    }
}
