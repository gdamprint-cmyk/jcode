//! Acceptance probe for the Unicode half-block Mermaid path.
//!
//! Why this exists: Windows Terminal 1.24 hosted by `herdr` exposes NEITHER
//! Kitty graphics NOR Sixel to the inner program — herdr's ghostty-vt consumes
//! the Sixel DCS `q` as an unknown escape sequence and only proxies Kitty
//! placements, which WT itself does not implement. So on that host jcode has no
//! native image protocol at all, and the ONLY way a Mermaid diagram can appear
//! is the Unicode half-block fallback that `ratatui-image` rasterizes into
//! cell characters itself.
//!
//! The gate that selects this path lives in `jcode-tui-markdown`
//! (`should_render_mermaid_block`), gated on `features.mermaid_halfblocks`.
//! The rendering itself lives here in the mermaid crate. This probe covers the
//! rendering half end to end through the public API:
//!
//!   1. force the picker to the Halfblocks protocol,
//!   2. render a real Mermaid diagram to a PNG on disk,
//!   3. draw it into a ratatui `Buffer` through the public viewport entry,
//!   4. assert the buffer contains non-blank, non-border cell content —
//!      i.e. the diagram actually became visible glyphs, not an empty region.
//!
//! A negative control asserts the same draw path leaves the buffer untouched
//! for an unknown hash, so the assertion cannot pass for the wrong reason
//! (e.g. a stray background fill or a border artifact).
//!
//! Run explicitly:
//!   cargo test -p jcode-tui-mermaid --test halfblocks_render -- --nocapture

// reference: https://github.com/benjajaja/ratatui-image/blob/main/src/protocol/halfblocks.rs
// (the upstream half-block encoder: `▀`/`▄` glyphs with fg/bg colors, and the
// `ProtocolTrait::render` loop that writes one HalfBlock per cell).
#![cfg(feature = "renderer")]

use jcode_tui_mermaid::{RenderResult, init_picker, render_mermaid_untracked};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// Force the global picker onto the Unicode half-block protocol.
///
/// `init_picker` is a `OnceLock` initializer, so it must run before any
/// protocol decision is taken. The environment here has no `KITTY_WINDOW_ID`
/// and a plain `TERM`, so the fast picker already starts on Halfblocks; the
/// assertion below makes that explicit rather than assumed.
fn ensure_halfblocks_picker() {
    init_picker();
    let protocol = jcode_tui_mermaid::protocol_type();
    assert!(
        matches!(
            protocol,
            Some(ratatui_image::picker::ProtocolType::Halfblocks)
        ),
        "probe requires the Halfblocks protocol, got {protocol:?}"
    );
}

fn render_diagram() -> u64 {
    let content = "flowchart LR\n    A[Start] --> B[Middle] --> C[End]";
    match render_mermaid_untracked(content, Some(80)) {
        RenderResult::Image { hash, path, .. } => {
            assert!(
                path.exists(),
                "rendered PNG must exist on disk: {}",
                path.display()
            );
            hash
        }
        RenderResult::Error(error) => panic!("mermaid render failed: {error}"),
    }
}

/// Count cells that carry visible content: any character that is not a space
/// and not the box-drawing border glyph the viewport draws around the image.
fn visible_cells(buf: &Buffer) -> usize {
    let mut count = 0;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let symbol = buf[(x, y)].symbol();
            if symbol.trim().is_empty() {
                continue;
            }
            if matches!(symbol, "│" | "┌" | "┐" | "└" | "┘" | "─" | "┃") {
                continue;
            }
            count += 1;
        }
    }
    count
}

#[test]
fn halfblocks_protocol_draws_mermaid_as_visible_cells() {
    ensure_halfblocks_picker();
    let hash = render_diagram();

    let area = Rect::new(0, 0, 80, 30);
    let mut buf = Buffer::empty(area);

    // Negative control: an unknown hash must not draw anything. If this
    // passed with content, the positive assertion below would prove nothing.
    let drew_unknown = jcode_tui_mermaid::render_image_widget_fit_stable(
        u64::MAX,
        area,
        &mut buf,
        80,
        30,
        0,
        false,
        false,
    );
    assert!(
        !drew_unknown,
        "unknown hash must not report a successful draw"
    );
    assert_eq!(
        visible_cells(&buf),
        0,
        "negative control: unknown hash must leave the buffer empty"
    );

    let drew = jcode_tui_mermaid::render_image_widget_fit_stable(
        hash, area, &mut buf, 80, 30, 0, false, false,
    );
    assert!(drew, "half-block draw must succeed for a rendered diagram");

    let visible = visible_cells(&buf);
    assert!(
        visible > 0,
        "half-block draw produced no visible cells: the diagram did not render"
    );

    // A half-block raster of a flowchart uses the upper/lower half block
    // characters (U+2580/U+2584). Requiring at least one of them proves the
    // buffer holds an actual rasterized image rather than stray text.
    let mut has_half_block = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let symbol = buf[(x, y)].symbol();
            if symbol.chars().any(|c| matches!(c, '\u{2580}' | '\u{2584}')) {
                has_half_block = true;
                break;
            }
        }
        if has_half_block {
            break;
        }
    }
    assert!(
        has_half_block,
        "expected at least one Unicode half-block character in the drawn buffer"
    );

    eprintln!("half-block draw produced {visible} visible cells");
}
