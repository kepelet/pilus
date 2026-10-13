//! The typefaces pilus ships, registered with GPUI's text system.
//!
//! The defaults (Geist for chrome, Ioskeley Mono Term for the grid) have to
//! render the same on a machine that has never installed them, so the faces
//! are compiled into the binary and handed to the text system before any
//! window shapes a line. They then list in the Appearances pickers beside the
//! system families.

use std::borrow::Cow;

use gpui::TextSystem;

/// Every bundled face. Only the weights the chrome and the grid actually ask
/// for: Geist for body, emphasis, and bold labels; the terminal's four styles.
const FACES: [&[u8]; 10] = [
    include_bytes!("../../assets/fonts/Geist-Regular.ttf"),
    include_bytes!("../../assets/fonts/Geist-Medium.ttf"),
    include_bytes!("../../assets/fonts/Geist-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/Geist-Bold.ttf"),
    include_bytes!("../../assets/fonts/Geist-Italic.ttf"),
    include_bytes!("../../assets/fonts/Geist-BoldItalic.ttf"),
    include_bytes!("../../assets/fonts/IoskeleyMonoTerm-Regular.ttf"),
    include_bytes!("../../assets/fonts/IoskeleyMonoTerm-Bold.ttf"),
    include_bytes!("../../assets/fonts/IoskeleyMonoTerm-Italic.ttf"),
    include_bytes!("../../assets/fonts/IoskeleyMonoTerm-BoldItalic.ttf"),
];

/// Register the bundled faces. A failure leaves the system fonts in charge,
/// which is a worse look rather than a broken window.
pub fn install(text_system: &TextSystem) {
    let faces = FACES.iter().map(|face| Cow::Borrowed(*face)).collect();
    if let Err(error) = text_system.add_fonts(faces) {
        eprintln!("pilus: could not load the bundled fonts: {error}");
    }
}
