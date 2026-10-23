use gtk::prelude::*;

// Shared native density: fields, cards, and dialogs use the same spacing roles.
pub const FIELD_GAP: i32 = 4;
pub const GAP: i32 = 8;
pub const GROUP_GAP: i32 = 16;
pub const INSET: i32 = 24;
pub const CONTROL_HEIGHT: i32 = 36;

pub fn inset(widget: &impl IsA<gtk::Widget>, space: i32) {
    widget.set_margin_top(space);
    widget.set_margin_bottom(space);
    widget.set_margin_start(space);
    widget.set_margin_end(space);
}

pub fn install() {
    let css = gtk::CssProvider::new();
    css.load_from_data(
        "@define-color rclone_muted mix(@theme_fg_color, @theme_bg_color, 0.35);
         @define-color rclone_card_fill alpha(@theme_fg_color, 0.05);
         @define-color rclone_card_edge alpha(@theme_fg_color, 0.12);
         .rclone-app .rclone-hint,
         window.rclone-profile entry text > placeholder,
         window.rclone-profile .rclone-hint {
             color: @rclone_muted;
             opacity: 1;
         }
         frame.rclone-mount-card {
             background-color: @rclone_card_fill;
             border: 1px solid @rclone_card_edge;
             border-radius: 8px;
         }
         /* Breeze's negative slider margins can produce a -2px GTK minimum. */
         .rclone-app scrollbar slider,
         .rclone-profile scrollbar slider,
         .rclone-log scrollbar slider {
             margin: 0;
         }
         /* Use the native symbolic X rather than theme-supplied titlebar artwork. */
         .rclone-app windowcontrols button.close,
         .rclone-profile windowcontrols button.close,
         .rclone-log windowcontrols button.close {
             background-image: none;
         }
         .rclone-app windowcontrols button.close image,
         .rclone-profile windowcontrols button.close image,
         .rclone-log windowcontrols button.close image {
             color: @theme_fg_color;
             -gtk-icon-source: -gtk-icontheme(\"window-close-symbolic\");
         }
         .rclone-app windowcontrols button.close:hover,
         .rclone-profile windowcontrols button.close:hover,
         .rclone-log windowcontrols button.close:hover {
             background-color: alpha(@theme_fg_color, 0.10);
         }
         .rclone-app windowcontrols button.close:active,
         .rclone-profile windowcontrols button.close:active,
         .rclone-log windowcontrols button.close:active {
             background-color: alpha(@theme_fg_color, 0.18);
         }",
    );
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("GTK application has a display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
