//! The app icon (15 §2): one PNG, used for the eframe windows (main window,
//! connection manager) and the viewer's winit window. The executable's own
//! icon comes from `assets/kanodesk.ico` through `build.rs` on Windows.

const PNG: &[u8] = include_bytes!("../assets/kanodesk-256.png");

/// The app icon for eframe windows.
pub fn egui_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(PNG).expect("the bundled icon is a valid PNG")
}

/// The app icon for the viewer's winit window.
pub fn winit_icon() -> Option<winit::window::Icon> {
    let i = egui_icon();
    winit::window::Icon::from_rgba(i.rgba, i.width, i.height).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_decodes() {
        let i = egui_icon();
        assert_eq!(i.width, i.height, "square");
        assert!(i.width >= 64);
        assert_eq!(i.rgba.len(), (i.width * i.height * 4) as usize);
        assert!(winit_icon().is_some());
    }
}
