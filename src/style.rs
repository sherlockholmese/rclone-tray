use gtk::prelude::*;

// Shared native density: fields, rows, and dialogs use the same spacing roles.
pub const FIELD_GAP: i32 = 4;
pub const GAP: i32 = 8;
pub const ROW_GAP: i32 = 12;
pub const GROUP_GAP: i32 = 16;
pub const INSET: i32 = 24;

pub fn inset(widget: &impl IsA<gtk::Widget>, vertical: i32, horizontal: i32) {
    widget.set_margin_top(vertical);
    widget.set_margin_bottom(vertical);
    widget.set_margin_start(horizontal);
    widget.set_margin_end(horizontal);
}

/// The desktop theme owns every color. These roles only derive from it, and mixing state
/// colors toward the foreground keeps them readable on light and dark themes alike.
fn tokens() -> String {
    let probe = gtk::Label::new(None).style_context();
    let state = |name: &str, fallback: &str| {
        if probe.lookup_color(name).is_some() {
            format!("@{name}")
        } else {
            fallback.to_string()
        }
    };
    format!(
        "@define-color rclone_muted mix(@theme_fg_color, @theme_bg_color, 0.30);
         @define-color rclone_rule alpha(@theme_fg_color, 0.10);
         @define-color rclone_hover alpha(@theme_fg_color, 0.05);
         @define-color rclone_press alpha(@theme_fg_color, 0.10);
         @define-color rclone_accent @theme_selected_bg_color;
         @define-color rclone_ok mix({}, @theme_fg_color, 0.20);
         @define-color rclone_warning mix({}, @theme_fg_color, 0.40);
         @define-color rclone_danger mix({}, @theme_fg_color, 0.30);
         @define-color rclone_notice alpha({}, 0.14);\n",
        state("success_color", "#2ec27e"),
        state("warning_color", "#c88800"),
        state("error_color", "#e01b24"),
        state("warning_color", "#c88800"),
    )
}

const COMPONENTS: &str = "
    .rclone-muted, .rclone-muted label { color: @rclone_muted; opacity: 1; }
    .rclone-numeric { font-feature-settings: 'tnum'; }
    label.rclone-ok { color: @rclone_muted; }
    label.rclone-idle { color: @rclone_muted; }
    label.rclone-busy { color: @rclone_muted; }
    label.rclone-warning, image.rclone-warning { color: @rclone_warning; }
    label.rclone-danger, image.rclone-danger { color: @rclone_danger; }

    /* The ledger: one plane of rows. Each row's lower edge is its transfer progress. */
    list.rclone-ledger, listview.rclone-ledger { background: none; }
    list.rclone-ledger > row {
        padding: 0;
        background: none;
        border: none;
        box-shadow: none;
        border-radius: 0;
        transition: background-color 120ms ease-out;
    }
    list.rclone-ledger > row:hover { background-color: @rclone_hover; }
    list.rclone-ledger > row:focus-visible {
        outline: 2px solid @theme_fg_color;
        outline-offset: -2px;
    }
    list.rclone-ledger > row:active { background-color: @rclone_press; transition: none; }
    listview.rclone-ledger > row { padding: 0; background: none; }
    .rclone-name { font-weight: 600; }
    .rclone-unmounted .rclone-name { font-weight: 400; color: @rclone_muted; }

    progressbar.rclone-seam { min-height: 2px; padding: 0; margin: 0; }
    progressbar.rclone-seam > trough {
        min-height: 2px;
        margin: 0;
        padding: 0;
        border: none;
        border-radius: 0;
        box-shadow: none;
        background-image: none;
        background-color: @rclone_rule;
    }
    progressbar.rclone-seam > trough > progress {
        min-height: 2px;
        margin: 0;
        border: none;
        border-radius: 0;
        box-shadow: none;
        background-image: none;
        background-color: @rclone_accent;
    }

    /* Status lamp: fill, ring, spinner, and warning icon differ by shape as well as color. */
    .rclone-dot { min-width: 8px; min-height: 8px; border-radius: 4px; }
    .rclone-dot.rclone-ok { background-color: @rclone_ok; }
    .rclone-dot.rclone-idle { box-shadow: inset 0 0 0 2px @rclone_muted; }

    .rclone-banner { background-color: @rclone_notice; }
    .rclone-headline { font-size: 1.3em; font-weight: 600; }
    .rclone-tabbar { box-shadow: inset 0 -2px @rclone_rule; }
    .rclone-tabs > button {
        min-height: 0;
        padding: 8px 12px;
        border: none;
        border-radius: 0;
        background: none;
        box-shadow: none;
        color: @rclone_muted;
    }
    .rclone-tabs > button label { color: inherit; }
    .rclone-tabs > button:hover { background-color: @rclone_hover; color: @theme_fg_color; }
    .rclone-tabs > button:active { background-color: @rclone_press; }
    .rclone-tabs > button:checked {
        background: none;
        color: @theme_fg_color;
        box-shadow: inset 0 -2px @rclone_accent;
    }
    .rclone-tabs > button:focus-visible {
        outline: 2px solid @theme_fg_color;
        outline-offset: -2px;
    }
    .rclone-empty-title { font-size: 1.3em; font-weight: 600; }
    .rclone-log { font-family: monospace; }
    .rclone-log text { background: none; }

    /* Breeze's negative slider margins can produce a -2px GTK minimum. */
    scrollbar slider { margin: 0; }

    /* Use the native symbolic X rather than theme-supplied titlebar artwork. */
    windowcontrols button.close { background-image: none; }
    windowcontrols button.close image {
        color: @theme_fg_color;
        -gtk-icon-source: -gtk-icontheme(\"window-close-symbolic\");
    }
    windowcontrols button.close:hover { background-color: alpha(@theme_fg_color, 0.10); }
    windowcontrols button.close:active { background-color: alpha(@theme_fg_color, 0.18); }
";

pub fn install() {
    let css = gtk::CssProvider::new();
    css.load_from_data(&(tokens() + COMPONENTS));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("GTK application has a display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
