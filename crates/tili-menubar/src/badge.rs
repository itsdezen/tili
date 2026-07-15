use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBaselineOffsetAttributeName, NSBezierPath, NSColor,
    NSCompositingOperation, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSGraphicsContext, NSImage,
};
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSMutableAttributedString, NSNumber,
    NSPoint, NSRect, NSSize, NSString,
};

/// Horizontal padding (points) on each side of the text inside the pill.
const PADDING_X: f64 = 7.0;
/// Fixed badge height — matches the typical menu bar content height so it
/// sits centered without extra vertical margin.
const HEIGHT: f64 = 16.0;
/// The leading dot's own font size — deliberately smaller than the
/// workspace name's (see `text_attributes`), landing between the tiny "•"
/// bullet glyph and a full-size "●" circle at the name's own font size.
const DOT_FONT_SIZE: f64 = 7.0;

/// Draws `text` as a solid rounded-rect pill with the text itself
/// "knocked out" (cut fully transparent) through the fill, so whatever's
/// behind the menu bar shows through in the shape of the letters — then
/// marks the result as a template image, so AppKit tints the opaque pill
/// area with the correct color for the current light/dark/highlighted
/// menu bar state automatically, same as any other menu bar icon.
pub fn image_for(text: &str) -> Retained<NSImage> {
    let attr_string = badge_attributed_string(text);
    let text_size = attr_string.size();

    let width = (text_size.width + PADDING_X * 2.0).max(HEIGHT);
    let size = NSSize {
        width,
        height: HEIGHT,
    };

    let draw = block2::RcBlock::new(move |rect: NSRect| -> Bool {
        let radius = rect.size.height / 2.0;
        let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius);
        NSColor::blackColor().set();
        path.fill();

        if let Some(ctx) = NSGraphicsContext::currentContext() {
            ctx.setCompositingOperation(NSCompositingOperation::DestinationOut);
        }
        let point = NSPoint {
            x: (rect.size.width - text_size.width) / 2.0,
            y: (rect.size.height - text_size.height) / 2.0,
        };
        attr_string.drawAtPoint(point);

        Bool::YES
    });

    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw);
    image.setTemplate(true);
    image
}

/// A leading filled circle at `DOT_FONT_SIZE`, followed by `text` at the
/// normal badge size — two runs so the dot can be sized independently of
/// the workspace name instead of both sharing one font size. Both runs
/// share one baseline by default (`NSAttributedString` always aligns
/// runs to a common baseline), which visually reads as the *smaller* dot
/// sitting low/off-center against the taller name text — `dot_baseline_offset`
/// shifts it up by half the cap-height difference between the two fonts
/// so the dot's own visual center lines up with the name text's instead.
fn badge_attributed_string(text: &str) -> Retained<NSAttributedString> {
    let name_font = NSFont::boldSystemFontOfSize(NSFont::smallSystemFontSize());
    let dot_font = NSFont::boldSystemFontOfSize(DOT_FONT_SIZE);
    let dot_baseline_offset = (name_font.capHeight() - dot_font.capHeight()) / 2.0;

    let dot = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str("● "),
            Some(&text_attributes(&dot_font, Some(dot_baseline_offset))),
        )
    };
    let name = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&text_attributes(&name_font, None)),
        )
    };
    let full = NSMutableAttributedString::new();
    full.appendAttributedString(&dot);
    full.appendAttributedString(&name);
    Retained::into_super(full)
}

fn text_attributes(
    font: &NSFont,
    baseline_offset: Option<f64>,
) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    let color = NSColor::blackColor();
    // SAFETY: reading these extern statics — Apple's own well-known
    // attribute-name constants, never mutated — is sound.
    let mut keys: Vec<&NSAttributedStringKey> =
        unsafe { vec![NSFontAttributeName, NSForegroundColorAttributeName] };
    let mut values: Vec<&AnyObject> = vec![font.as_ref(), color.as_ref()];
    let offset_number;
    if let Some(offset) = baseline_offset {
        offset_number = NSNumber::new_f64(offset);
        keys.push(unsafe { NSBaselineOffsetAttributeName });
        values.push(offset_number.as_ref());
    }
    NSDictionary::from_slices(&keys, &values)
}
