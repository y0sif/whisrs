//! X11 override-redirect overlay backend.

use std::sync::mpsc;
use std::time::Duration;

use tiny_skia::{Pixmap, PremultipliedColorU8};
use tracing::{info, warn};
use x11rb::connection::{Connection as X11Connection, RequestConnection};
use x11rb::protocol::{
    randr::{ConnectionExt as X11RandrConnectionExt, MonitorInfo, X11_EXTENSION_NAME as RANDR_EXT},
    shape::{ConnectionExt as X11ShapeConnectionExt, SK, SO, X11_EXTENSION_NAME as SHAPE_EXT},
    xproto::{
        AtomEnum, ClipOrdering, ColormapAlloc, ConfigureWindowAux,
        ConnectionExt as X11ProtoConnectionExt, CreateGCAux, CreateWindowAux, EventMask,
        ImageFormat, PropMode, Rectangle, Screen, StackMode, VisualClass, Visualid, Window,
        WindowClass,
    },
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as X11WrapperConnectionExt;

use crate::{OverlayConfig, State};

use super::render::{OverlayRenderer, Theme, BOTTOM_MARGIN, FRAME_MS};
use super::service::OverlayError;

pub(super) fn run_x11_overlay(
    state_rx: mpsc::Receiver<State>,
    level_rx: mpsc::Receiver<f32>,
    config: OverlayConfig,
) -> Result<(), OverlayError> {
    let width = config.clamped_width() as u16;
    let height = config.clamped_height() as u16;
    let theme = Theme::from_config(&config);

    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let visual = find_argb_visual(screen).ok_or(OverlayError::X11Visual)?;
    let atoms = X11Atoms::new(&conn)?;

    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual.visual_id)?;

    let x = centered_x(screen, width);
    let y = bottom_y(screen, height);
    let window = conn.generate_id()?;
    conn.create_window(
        visual.depth,
        window,
        screen.root,
        x,
        y,
        width,
        height,
        0,
        WindowClass::INPUT_OUTPUT,
        visual.visual_id,
        &CreateWindowAux::default()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE),
    )?;

    set_x11_window_hints(&conn, window, &atoms)?;
    let shape_supported = make_x11_window_click_through(&conn, window)?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, window, &CreateGCAux::default().graphics_exposures(0))?;
    conn.flush()?;

    let mut renderer =
        OverlayRenderer::new(state_rx, level_rx, width as u32, height as u32, theme)?;
    let mut frame = vec![0_u8; width as usize * height as usize * 4];
    let mut mapped = false;
    // Cache of the last bounding-shape rectangles applied to the window.
    // Recomputing alpha_shape_rectangles every frame is cheap, but issuing
    // the SHAPE round-trip when geometry hasn't changed is not — skip it.
    let mut last_shape: Vec<Rectangle> = Vec::new();

    info!("recording X11 overlay started");
    loop {
        // Drain the event queue. Redraws are time-driven (the loop below
        // pushes a frame every `FRAME_MS`), so we don't react to `Expose`
        // explicitly — we just consume events so the protocol stream
        // stays well-formed.
        while conn.poll_for_event()?.is_some() {}

        renderer.draw_frame();
        if renderer.disconnected {
            break;
        }
        let visible_shape = alpha_shape_rectangles(&renderer.pixmap);
        if visible_shape.is_empty() {
            if mapped {
                conn.unmap_window(window)?;
                conn.flush()?;
                mapped = false;
                last_shape.clear();
            }
            std::thread::sleep(Duration::from_millis(FRAME_MS));
            continue;
        }

        if shape_supported && !shape_rectangles_eq(&visible_shape, &last_shape) {
            set_x11_bounding_shape(&conn, window, &visible_shape)?;
            last_shape.clear();
            last_shape.extend_from_slice(&visible_shape);
        }
        copy_pixmap_to_x11_zpixmap(&renderer.pixmap, visual, &mut frame);
        if !mapped {
            let (x, y) = x11_overlay_position(&conn, screen, &atoms, width, height);
            conn.configure_window(
                window,
                &ConfigureWindowAux::default()
                    .x(i32::from(x))
                    .y(i32::from(y))
                    .stack_mode(StackMode::ABOVE),
            )?;
            conn.map_window(window)?;
            mapped = true;
        }
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            window,
            gc,
            width,
            height,
            0,
            0,
            0,
            visual.depth,
            &frame,
        )?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(FRAME_MS));
    }

    if mapped {
        let _ = conn.unmap_window(window);
        let _ = conn.flush();
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct X11ArgbVisual {
    depth: u8,
    visual_id: Visualid,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
}

fn find_argb_visual(screen: &Screen) -> Option<X11ArgbVisual> {
    screen.allowed_depths.iter().find_map(|depth| {
        if depth.depth != 32 {
            return None;
        }
        let full_mask = depth_mask(depth.depth);
        depth.visuals.iter().find_map(|visual| {
            if visual.class != VisualClass::TRUE_COLOR {
                return None;
            }
            let color_mask = visual.red_mask | visual.green_mask | visual.blue_mask;
            let alpha_mask = full_mask & !color_mask;
            (alpha_mask != 0).then_some(X11ArgbVisual {
                depth: depth.depth,
                visual_id: visual.visual_id,
                red_mask: visual.red_mask,
                green_mask: visual.green_mask,
                blue_mask: visual.blue_mask,
                alpha_mask,
            })
        })
    })
}

fn depth_mask(depth: u8) -> u32 {
    if depth >= 32 {
        u32::MAX
    } else {
        (1_u32 << depth) - 1
    }
}

struct X11Atoms {
    atom: u32,
    net_active_window: u32,
    net_wm_name: u32,
    net_wm_window_type: u32,
    net_wm_window_type_notification: u32,
    net_wm_state: u32,
    net_wm_state_above: u32,
    net_wm_state_skip_pager: u32,
    net_wm_state_skip_taskbar: u32,
    net_wm_state_sticky: u32,
    net_wm_bypass_compositor: u32,
    utf8_string: u32,
}

impl X11Atoms {
    fn new(conn: &RustConnection) -> Result<Self, OverlayError> {
        Ok(Self {
            atom: u32::from(AtomEnum::ATOM),
            net_active_window: intern_atom(conn, b"_NET_ACTIVE_WINDOW")?,
            net_wm_name: intern_atom(conn, b"_NET_WM_NAME")?,
            net_wm_window_type: intern_atom(conn, b"_NET_WM_WINDOW_TYPE")?,
            net_wm_window_type_notification: intern_atom(
                conn,
                b"_NET_WM_WINDOW_TYPE_NOTIFICATION",
            )?,
            net_wm_state: intern_atom(conn, b"_NET_WM_STATE")?,
            net_wm_state_above: intern_atom(conn, b"_NET_WM_STATE_ABOVE")?,
            net_wm_state_skip_pager: intern_atom(conn, b"_NET_WM_STATE_SKIP_PAGER")?,
            net_wm_state_skip_taskbar: intern_atom(conn, b"_NET_WM_STATE_SKIP_TASKBAR")?,
            net_wm_state_sticky: intern_atom(conn, b"_NET_WM_STATE_STICKY")?,
            net_wm_bypass_compositor: intern_atom(conn, b"_NET_WM_BYPASS_COMPOSITOR")?,
            utf8_string: intern_atom(conn, b"UTF8_STRING")?,
        })
    }
}

fn intern_atom(conn: &RustConnection, name: &[u8]) -> Result<u32, OverlayError> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn set_x11_window_hints(
    conn: &RustConnection,
    window: Window,
    atoms: &X11Atoms,
) -> Result<(), OverlayError> {
    conn.change_property8(
        PropMode::REPLACE,
        window,
        atoms.net_wm_name,
        atoms.utf8_string,
        b"whisrs overlay",
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"whisrs overlay",
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms.net_wm_window_type,
        atoms.atom,
        &[atoms.net_wm_window_type_notification],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms.net_wm_state,
        atoms.atom,
        &[
            atoms.net_wm_state_above,
            atoms.net_wm_state_skip_pager,
            atoms.net_wm_state_skip_taskbar,
            atoms.net_wm_state_sticky,
        ],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms.net_wm_bypass_compositor,
        AtomEnum::CARDINAL,
        &[2],
    )?;
    Ok(())
}

fn make_x11_window_click_through(
    conn: &RustConnection,
    window: Window,
) -> Result<bool, OverlayError> {
    if conn.extension_information(SHAPE_EXT)?.is_none() {
        warn!("X11 SHAPE extension unavailable; overlay may intercept clicks");
        return Ok(false);
    }

    conn.shape_rectangles(
        SO::SET,
        SK::INPUT,
        ClipOrdering::UNSORTED,
        window,
        0,
        0,
        &[],
    )?;
    Ok(true)
}

fn set_x11_bounding_shape(
    conn: &RustConnection,
    window: Window,
    rectangles: &[Rectangle],
) -> Result<(), OverlayError> {
    conn.shape_rectangles(
        SO::SET,
        SK::BOUNDING,
        ClipOrdering::Y_SORTED,
        window,
        0,
        0,
        rectangles,
    )?;
    Ok(())
}

fn shape_rectangles_eq(a: &[Rectangle], b: &[Rectangle]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(lhs, rhs)| {
        lhs.x == rhs.x && lhs.y == rhs.y && lhs.width == rhs.width && lhs.height == rhs.height
    })
}

fn alpha_shape_rectangles(pixmap: &Pixmap) -> Vec<Rectangle> {
    let width = pixmap.width() as usize;
    let mut rectangles = Vec::new();

    for (y, row) in pixmap.pixels().chunks_exact(width).enumerate() {
        let Some(first) = row.iter().position(|px| px.alpha() != 0) else {
            continue;
        };
        let last = row
            .iter()
            .rposition(|px| px.alpha() != 0)
            .expect("row has a first alpha pixel");
        rectangles.push(Rectangle {
            x: first as i16,
            y: y as i16,
            width: (last - first + 1) as u16,
            height: 1,
        });
    }

    rectangles
}

fn x11_overlay_position(
    conn: &RustConnection,
    screen: &Screen,
    atoms: &X11Atoms,
    width: u16,
    height: u16,
) -> (i16, i16) {
    let monitors = match active_x11_monitors(conn, screen.root) {
        Ok(monitors) => monitors,
        Err(e) => {
            warn!("failed to query XRandR monitors for overlay placement: {e:#}");
            Vec::new()
        }
    };

    if let Some((cx, cy)) = match active_x11_window_center(conn, screen.root, atoms) {
        Ok(center) => center,
        Err(e) => {
            warn!("failed to query active X11 window for overlay placement: {e:#}");
            None
        }
    } {
        if let Some(monitor) = monitors
            .iter()
            .find(|m| point_in_rect(cx, cy, i32::from(m.x), i32::from(m.y), m.width, m.height))
        {
            return position_in_rect(
                i32::from(monitor.x),
                i32::from(monitor.y),
                u32::from(monitor.width),
                u32::from(monitor.height),
                width,
                height,
            );
        }
    }

    if let Some(monitor) = monitors
        .iter()
        .find(|m| m.primary)
        .or_else(|| monitors.first())
    {
        return position_in_rect(
            i32::from(monitor.x),
            i32::from(monitor.y),
            u32::from(monitor.width),
            u32::from(monitor.height),
            width,
            height,
        );
    }

    (centered_x(screen, width), bottom_y(screen, height))
}

fn active_x11_monitors(
    conn: &RustConnection,
    root: Window,
) -> Result<Vec<MonitorInfo>, OverlayError> {
    if conn.extension_information(RANDR_EXT)?.is_none() {
        return Ok(Vec::new());
    }

    let reply = conn.randr_get_monitors(root, true)?.reply()?;
    Ok(reply
        .monitors
        .into_iter()
        .filter(|monitor| monitor.width > 0 && monitor.height > 0)
        .collect())
}

fn active_x11_window_center(
    conn: &RustConnection,
    root: Window,
    atoms: &X11Atoms,
) -> Result<Option<(i32, i32)>, OverlayError> {
    let reply = conn
        .get_property(false, root, atoms.net_active_window, AtomEnum::WINDOW, 0, 1)?
        .reply()?;

    let Some(bytes) = reply.value.get(..4) else {
        return Ok(None);
    };
    let window = u32::from_ne_bytes(bytes.try_into().expect("slice is exactly 4 bytes"));
    if window == 0 {
        return Ok(None);
    }

    let geometry = conn.get_geometry(window)?.reply()?;
    let translated = conn.translate_coordinates(window, root, 0, 0)?.reply()?;
    if !translated.same_screen {
        return Ok(None);
    }

    Ok(Some((
        i32::from(translated.dst_x) + i32::from(geometry.width / 2),
        i32::from(translated.dst_y) + i32::from(geometry.height / 2),
    )))
}

fn point_in_rect(x: i32, y: i32, rx: i32, ry: i32, width: u16, height: u16) -> bool {
    let right = rx.saturating_add(i32::from(width));
    let bottom = ry.saturating_add(i32::from(height));
    x >= rx && x < right && y >= ry && y < bottom
}

fn position_in_rect(
    x: i32,
    y: i32,
    rect_width: u32,
    rect_height: u32,
    width: u16,
    height: u16,
) -> (i16, i16) {
    let width = i32::from(width);
    let height = i32::from(height);
    let rect_width = i32::try_from(rect_width).unwrap_or(i32::MAX);
    let rect_height = i32::try_from(rect_height).unwrap_or(i32::MAX);

    let min_x = x;
    let max_x = x
        .saturating_add(rect_width)
        .saturating_sub(width)
        .max(min_x);
    let centered_x = x.saturating_add((rect_width - width) / 2);

    let min_y = y;
    let max_y = y
        .saturating_add(rect_height)
        .saturating_sub(height)
        .max(min_y);
    let bottom_y = y
        .saturating_add(rect_height)
        .saturating_sub(height)
        .saturating_sub(BOTTOM_MARGIN);

    (
        to_i16_coord(centered_x.clamp(min_x, max_x)),
        to_i16_coord(bottom_y.clamp(min_y, max_y)),
    )
}

fn centered_x(screen: &Screen, width: u16) -> i16 {
    let x = (i32::from(screen.width_in_pixels) - i32::from(width)) / 2;
    to_i16_coord(x.max(0))
}

fn bottom_y(screen: &Screen, height: u16) -> i16 {
    let y = i32::from(screen.height_in_pixels) - i32::from(height) - BOTTOM_MARGIN;
    to_i16_coord(y.max(0))
}

fn to_i16_coord(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn copy_pixmap_to_x11_zpixmap(pixmap: &Pixmap, visual: X11ArgbVisual, canvas: &mut [u8]) {
    let src = pixmap.pixels();
    debug_assert_eq!(src.len() * 4, canvas.len());
    for (i, px) in src.iter().enumerate() {
        let pre: PremultipliedColorU8 = *px;
        let pixel = pack_masked_channel(pre.red(), visual.red_mask)
            | pack_masked_channel(pre.green(), visual.green_mask)
            | pack_masked_channel(pre.blue(), visual.blue_mask)
            | pack_masked_channel(pre.alpha(), visual.alpha_mask);
        canvas[i * 4..i * 4 + 4].copy_from_slice(&pixel.to_ne_bytes());
    }
}

/// Pack an 8-bit channel `value` into the bits described by `mask`, scaling
/// it from `[0, 255]` into `[0, mask_max]`. The `(value * max + 127) / 255`
/// form is the textbook integer rescale: adding `127` before dividing by
/// `255` performs round-half-up so the brightest input (`0xFF`) always
/// produces the brightest output, and intermediate values are nearest-
/// rounded instead of truncated toward zero.
fn pack_masked_channel(value: u8, mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let bits = mask.count_ones();
    let shift = mask.trailing_zeros();
    let max = (1_u64 << bits) - 1;
    let scaled = (u64::from(value) * max + 127) / 255;
    ((scaled << shift) as u32) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_position_in_rect_uses_monitor_bounds() {
        assert_eq!(
            position_in_rect(1920, 360, 1920, 1200, 100, 40),
            (2830, 1504)
        );
        assert_eq!(position_in_rect(3840, 0, 1200, 1920, 100, 40), (4390, 1864));
    }

    #[test]
    fn x11_point_in_rect_uses_half_open_bounds() {
        assert!(point_in_rect(2830, 1504, 1920, 360, 1920, 1200));
        assert!(!point_in_rect(3840, 1504, 1920, 360, 1920, 1200));
    }
}
