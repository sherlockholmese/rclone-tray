//! The window's motion roles. Durations, curves, and travel come from the theme's motion tokens,
//! so every surface moves the same way; screens use these roles instead of raw timings.
//!
//! Three patterns, each with one job:
//! - `reveal`: a plane whose content changed (another mount, another tab) fades in and settles
//!   a short distance, so the change reads as cause and effect rather than a flash.
//! - `fill`: a progress rail glides between samples instead of jumping once a second.
//! - dialogs fade and settle in place (`DIALOG`), rather than sliding from the window top.
//!
//! All of them resolve to their final state when the desktop asks for reduced motion.
use gpui_kit::base::motion::{Spring, spring};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::dialog::DialogEntrance;
use gpui_kit::{
    Animation, AnimationElement, AnimationExt as _, App, ElementId, IntoElement, Styled, Window, px,
};
use std::time::Duration;

/// Confirmations and the mount editor appear where they settle.
pub const DIALOG: DialogEntrance = DialogEntrance::FadeSlide;

/// Rails get a new value about once a second. A critically damped spring that settles in about
/// that time makes steady transfers read as steady, and never overshoots the real progress.
const PROGRESS: Spring = Spring::new(Duration::from_millis(900));

/// Fades a content plane in and settles it from just below, whenever `key` changes. The same
/// key keeps the plane at rest, so live updates never replay the reveal.
pub fn reveal<E>(
    key: impl Into<ElementId>,
    element: E,
    window: &Window,
    cx: &App,
) -> AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    let motion = &cx.theme().motion;
    let easing = motion.easing_enter.clone();
    let travel = f32::from(motion.distance_short.to_pixels(window.rem_size()));
    element.relative().with_animation(
        key,
        Animation::new(motion.duration_normal).with_easing(move |t| easing.sample(t)),
        move |this, t| this.opacity(t).top(px(travel * (1.0 - t))),
    )
}

/// The width a progress rail shows now, gliding toward `fraction`. A rail that just appeared
/// starts at its real value rather than filling from zero.
pub fn fill(key: impl Into<ElementId>, fraction: f64, window: &mut Window, cx: &mut App) -> f32 {
    spring(key.into(), fraction as f32, PROGRESS, window, cx).clamp(0.0, 1.0)
}

/// Follows the desktop's animation preference. GNOME-style desktops (and most others that ship
/// its schemas) expose it as `enable-animations`; KDE as an animation speed of zero.
pub struct Preference {
    gnome: Option<gio::Settings>,
    kde: Option<std::path::PathBuf>,
}

impl Preference {
    pub fn new() -> Self {
        let gnome = gio::SettingsSchemaSource::default()
            .and_then(|source| source.lookup("org.gnome.desktop.interface", true))
            .filter(|schema| schema.has_key("enable-animations"))
            .map(|_| gio::Settings::new("org.gnome.desktop.interface"));
        let kde = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
            })
            .map(|config| config.join("kdeglobals"));
        Self { gnome, kde }
    }

    /// Reads the preference again and applies it. Cheap enough to call on a slow timer.
    pub fn apply(&self, cx: &mut App) {
        cx.set_reduce_motion(self.reduced());
    }

    fn reduced(&self) -> bool {
        use gio::prelude::SettingsExt as _;
        if let Some(settings) = &self.gnome
            && !settings.boolean("enable-animations")
        {
            return true;
        }
        self.kde
            .as_ref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .is_some_and(|text| kde_disables_animation(&text))
    }
}

/// `AnimationDurationFactor=0` under `[KDE]` turns animations off.
fn kde_disables_animation(text: &str) -> bool {
    let mut in_kde = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_kde = line == "[KDE]";
        } else if in_kde && let Some(value) = line.strip_prefix("AnimationDurationFactor=") {
            return value
                .trim()
                .parse::<f32>()
                .is_ok_and(|factor| factor <= 0.0);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::kde_disables_animation;

    #[test]
    fn kde_animation_factor_zero_means_reduced_motion() {
        assert!(kde_disables_animation("[KDE]\nAnimationDurationFactor=0\n"));
        assert!(!kde_disables_animation(
            "[KDE]\nAnimationDurationFactor=0.5\n"
        ));
        assert!(!kde_disables_animation(
            "[General]\nAnimationDurationFactor=0\n"
        ));
        assert!(!kde_disables_animation(""));
    }
}
