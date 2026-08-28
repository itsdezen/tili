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
/// Corner radius for `MenubarShape::Rounded` — a smaller, fixed radius than
/// `Pill`'s fully-rounded ends (`HEIGHT / 2.0`), reading as an ordinary
/// rounded-rect button instead of a capsule.
const ROUNDED_RADIUS: f64 = 4.0;
/// Border width for `MenubarFill::Outlined` — the stroked path is inset by
/// half this so the stroke doesn't get clipped at the image edge (an
/// `NSBezierPath` stroke draws centered on the path).
const STROKE_WIDTH: f64 = 1.2;
/// The "main" mode dot's own font size — deliberately smaller than the
/// workspace name's (see `text_attributes`), landing between the tiny "•"
/// bullet glyph and a full-size "●" circle at the name's own font size.
const DOT_FONT_SIZE: f64 = 7.0;
/// The `resize` mode glyph's font size — larger than `DOT_FONT_SIZE` so it
/// reads clearly at menu-bar scale, while staying under the name font's
/// line height so it doesn't grow the pill past `HEIGHT`. The `manage`
/// glyph doesn't use this constant — it matches the workspace name's own
/// font size instead (see `glyph_font_size_for_mode`). The connecting-state
/// spinner (`image_for_connecting`) also uses this size.
const MODE_GLYPH_FONT_SIZE: f64 = 10.0;

/// Animated frames for the "connecting" badge (see `image_for_connecting`)
/// — a standard braille dot-spinner. Advanced by the caller (`menu.rs`)
/// off its own existing timer tick; this module just renders one frame at
/// a time.
const SPINNER_FRAMES: [&str; 10] = [
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];

/// Draws `text` as a rounded pill (see `tili_ipc::MenubarShape`), either
/// solid with the text/glyph "knocked out" as transparency
/// (`MenubarFill::Filled`, the default) or as a stroked outline with the
/// text/glyph drawn normally inside it (`MenubarFill::Outlined`) — then
/// marks the result as a template image, so AppKit tints the opaque area
/// with the correct color for the current light/dark/highlighted menu bar
/// state automatically, same as any other menu bar icon.
/// `style.uppercase` uppercases `text` before layout — not applied to the
/// dropdown menu's own item labels, which stay as-typed for readability.
pub fn image_for(text: &str, mode: &str, style: &tili_ipc::MenubarStyle) -> Retained<NSImage> {
    let text = if style.uppercase {
        text.to_uppercase()
    } else {
        text.to_string()
    };
    pill_image(
        badge_attributed_string(&text, mode),
        style.fill,
        style.shape,
    )
}

/// Same pill rendering as `image_for`, but for the "daemon unreachable,
/// retrying" state: a fixed `"tili"` label (no real workspace data exists)
/// with a spinning dot-spinner glyph in place of the mode dot — reads as
/// "connecting" rather than "broken", since a transient reconnect is the
/// common case (see `MAX_CONSECUTIVE_FAILURES`'s doc comment in
/// `main.rs`). `frame` indexes `SPINNER_FRAMES`, wrapping via `%` so the
/// caller doesn't need to track the modulus itself. Deliberately ignores
/// `MenubarStyle` — a user's custom fill/shape must not also become the
/// "everything's fine" connecting indicator, which would defeat the point
/// of it being visually distinct, so this always renders the built-in
/// filled pill look.
pub fn image_for_connecting(frame: usize) -> Retained<NSImage> {
    let glyph = format!("{} ", SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]);
    pill_image(
        attributed_string("tili", &glyph, MODE_GLYPH_FONT_SIZE),
        tili_ipc::MenubarFill::Filled,
        tili_ipc::MenubarShape::Pill,
    )
}

/// The pill drawing path shared by `image_for` and `image_for_connecting`
/// — takes ownership of `attr_string` since it must move into the
/// `'static` drawing block. Always a single (template-tinted) color;
/// there is no user-configurable badge color, only fill style and shape.
fn pill_image(
    attr_string: Retained<NSAttributedString>,
    fill: tili_ipc::MenubarFill,
    shape: tili_ipc::MenubarShape,
) -> Retained<NSImage> {
    let text_size = attr_string.size();

    let width = (text_size.width + PADDING_X * 2.0).max(HEIGHT);
    let size = NSSize {
        width,
        height: HEIGHT,
    };
    let radius = match shape {
        tili_ipc::MenubarShape::Pill => HEIGHT / 2.0,
        tili_ipc::MenubarShape::Rounded => ROUNDED_RADIUS,
    };

    let draw = block2::RcBlock::new(move |rect: NSRect| -> Bool {
        let text_point = NSPoint {
            x: (rect.size.width - text_size.width) / 2.0,
            y: (rect.size.height - text_size.height) / 2.0,
        };
        match fill {
            tili_ipc::MenubarFill::Filled => {
                let path =
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius);
                NSColor::blackColor().set();
                path.fill();

                if let Some(ctx) = NSGraphicsContext::currentContext() {
                    ctx.setCompositingOperation(NSCompositingOperation::DestinationOut);
                }
                attr_string.drawAtPoint(text_point);
            }
            tili_ipc::MenubarFill::Outlined => {
                let half_stroke = STROKE_WIDTH / 2.0;
                let stroke_rect = NSRect {
                    origin: NSPoint {
                        x: rect.origin.x + half_stroke,
                        y: rect.origin.y + half_stroke,
                    },
                    size: NSSize {
                        width: rect.size.width - STROKE_WIDTH,
                        height: rect.size.height - STROKE_WIDTH,
                    },
                };
                let stroke_radius = (radius - half_stroke).max(0.0);
                let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    stroke_rect,
                    stroke_radius,
                    stroke_radius,
                );
                path.setLineWidth(STROKE_WIDTH);
                NSColor::blackColor().set();
                path.stroke();

                attr_string.drawAtPoint(text_point);
            }
        }

        Bool::YES
    });

    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw);
    image.setTemplate(true);
    image
}

/// The leading glyph for each keybindings mode — `"main"` (the default,
/// nothing special active) keeps the plain dot; `resize`/`manage` (the
/// two built-in modes, see `example/tili.kdl`) get a distinct glyph so
/// the badge visibly reflects which one is active. Any other mode name a
/// user declares in their own config falls back to the plain dot, same
/// as `"main"`, rather than guessing at an unrecognized custom mode's
/// intent.
fn glyph_for_mode(mode: &str) -> &'static str {
    match mode {
        "resize" => "\u{2194} ",
        "manage" => "\u{2699} ",
        _ => "\u{25CF} ",
    }
}

/// The leading glyph's font size for each mode — `"manage"` matches
/// `name_font_point_size` exactly (the workspace name's own font size) so
/// its glyph reads at the same height as the name text; `"resize"` gets
/// `MODE_GLYPH_FONT_SIZE` so it still reads clearly without growing the
/// pill past `HEIGHT`; `"main"` and any unrecognized custom mode keep the
/// plain dot at `DOT_FONT_SIZE`, same fallback behavior as
/// `glyph_for_mode`.
fn glyph_font_size_for_mode(mode: &str, name_font_point_size: f64) -> f64 {
    match mode {
        "manage" => name_font_point_size,
        "resize" => MODE_GLYPH_FONT_SIZE,
        _ => DOT_FONT_SIZE,
    }
}

/// The normal (connected) badge's glyph + name text, resolved from the
/// current keybindings mode.
fn badge_attributed_string(text: &str, mode: &str) -> Retained<NSAttributedString> {
    let name_font_size = NSFont::smallSystemFontSize();
    attributed_string(
        text,
        glyph_for_mode(mode),
        glyph_font_size_for_mode(mode, name_font_size),
    )
}

/// A leading filled glyph at `glyph_font_size` (see `glyph_for_mode`/
/// `glyph_font_size_for_mode` for the connected badge, or
/// `image_for_connecting` for the spinner), followed by `text` at the
/// normal badge size — two runs so the glyph can be sized independently of
/// the workspace name instead of both sharing one font size. Both runs
/// share one baseline by default (`NSAttributedString` always aligns runs
/// to a common baseline), which visually reads as the *smaller* glyph
/// sitting low/off-center against the taller name text — the computed
/// baseline offset shifts it up by half the cap-height difference between
/// the two fonts so the glyph's own visual center lines up with the name
/// text's instead. That formula holds regardless of which font size the
/// glyph run uses, since it's computed from the two fonts' actual cap
/// heights rather than a fixed constant.
fn attributed_string(
    text: &str,
    glyph: &str,
    glyph_font_size: f64,
) -> Retained<NSAttributedString> {
    let name_font = NSFont::boldSystemFontOfSize(NSFont::smallSystemFontSize());
    let glyph_font = NSFont::boldSystemFontOfSize(glyph_font_size);
    let glyph_baseline_offset = (name_font.capHeight() - glyph_font.capHeight()) / 2.0;

    let glyph = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(glyph),
            Some(&text_attributes(&glyph_font, Some(glyph_baseline_offset))),
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
    full.appendAttributedString(&glyph);
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
