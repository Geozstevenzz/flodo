//! Native resizing for the undecorated window. Hit areas are logical points,
//! so they stay usable on high-DPI monitors without changing the drawn border.
use eframe::egui::{self, CursorIcon, Pos2, Rect, ResizeDirection};

const EDGE: f32 = 8.0;
const CORNER: f32 = 20.0;
pub const MIN_SIZE: [f32; 2] = [260.0, 180.0];

#[cfg(any(target_os = "macos", test))]
fn resized_rect(start: Rect, direction: ResizeDirection, delta: egui::Vec2) -> Rect {
    use ResizeDirection::*;
    let mut rect = start;
    if matches!(direction, West | NorthWest | SouthWest) {
        rect.min.x = (start.min.x + delta.x).min(start.max.x - MIN_SIZE[0]);
    }
    if matches!(direction, East | NorthEast | SouthEast) {
        rect.max.x = (start.max.x + delta.x).max(start.min.x + MIN_SIZE[0]);
    }
    if matches!(direction, North | NorthWest | NorthEast) {
        rect.min.y = (start.min.y + delta.y).min(start.max.y - MIN_SIZE[1]);
    }
    if matches!(direction, South | SouthWest | SouthEast) {
        rect.max.y = (start.max.y + delta.y).max(start.min.y + MIN_SIZE[1]);
    }
    rect
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct ResizeGesture {
    rect: Rect,
    pointer: Pos2,
    direction: ResizeDirection,
}

#[cfg(target_os = "macos")]
fn begin_resize(ui: &egui::Ui, direction: ResizeDirection) {
    // winit does not implement drag_resize_window on macOS. Undecorated
    // windows have matching inner/outer frames, so retain desktop coordinates
    // and resize from the original press without accumulating rounding errors.
    let gesture = ui.input(|i| {
        let rect = i.viewport().inner_rect?;
        Some(ResizeGesture {
            rect,
            pointer: rect.min + i.pointer.interact_pos()?.to_vec2(),
            direction,
        })
    });
    if let Some(gesture) = gesture {
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("native-resize"), gesture));
    }
}

#[cfg(not(target_os = "macos"))]
fn begin_resize(ui: &egui::Ui, direction: ResizeDirection) {
    ui.ctx()
        .send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
}

#[cfg(target_os = "macos")]
fn continue_resize(ui: &egui::Ui) {
    let id = egui::Id::new("native-resize");
    if !ui.input(|i| i.pointer.primary_down()) {
        ui.ctx().data_mut(|d| d.remove::<ResizeGesture>(id));
        return;
    }
    let gesture = ui.ctx().data(|d| d.get_temp::<ResizeGesture>(id));
    if let Some(gesture) = gesture {
        let current = ui.input(|i| {
            let rect = i.viewport().inner_rect?;
            Some((rect, rect.min + i.pointer.interact_pos()?.to_vec2()))
        });
        if let Some((current_rect, pointer)) = current {
            let rect = resized_rect(gesture.rect, gesture.direction, pointer - gesture.pointer);
            if rect.min != current_rect.min {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::OuterPosition(rect.min));
            }
            if rect.size() != current_rect.size() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::InnerSize(rect.size()));
            }
        }
    }
}

fn resize_zones(bounds: Rect) -> Vec<(Rect, ResizeDirection)> {
    use ResizeDirection::*;
    let (l, t, r, b) = (bounds.left(), bounds.top(), bounds.right(), bounds.bottom());
    // Corners are L-shaped, leaving title-bar controls inside the content
    // margin clickable while providing a longer diagonal-resize target.
    let segments = [
        (l, t, l + CORNER, t + EDGE, NorthWest),
        (l + CORNER, t, r - CORNER, t + EDGE, North),
        (r - CORNER, t, r, t + EDGE, NorthEast),
        (l, b - EDGE, l + CORNER, b, SouthWest),
        (l + CORNER, b - EDGE, r - CORNER, b, South),
        (r - CORNER, b - EDGE, r, b, SouthEast),
        (l, t + EDGE, l + EDGE, t + CORNER, NorthWest),
        (l, t + CORNER, l + EDGE, b - CORNER, West),
        (l, b - CORNER, l + EDGE, b - EDGE, SouthWest),
        (r - EDGE, t + EDGE, r, t + CORNER, NorthEast),
        (r - EDGE, t + CORNER, r, b - CORNER, East),
        (r - EDGE, b - CORNER, r, b - EDGE, SouthEast),
    ];
    segments
        .into_iter()
        .map(|(x1, y1, x2, y2, d)| (Rect::from_min_max(Pos2::new(x1, y1), Pos2::new(x2, y2)), d))
        .collect()
}

pub fn resize_frame(ui: &mut egui::Ui) {
    if ui.input(|i| i.viewport().maximized == Some(true) || i.viewport().fullscreen == Some(true)) {
        return;
    }
    for (index, (rect, direction)) in resize_zones(ui.ctx().viewport_rect())
        .into_iter()
        .enumerate()
    {
        let response = ui.interact(
            rect,
            egui::Id::new(("window-resize", index)),
            egui::Sense::drag(),
        );
        if response.hovered() || response.dragged() {
            let cursor = match direction {
                ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
                ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
                ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
                ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
            };
            ui.ctx().set_cursor_icon(cursor);
        }
        // Sense::drag starts on press, while the OS still has the button
        // event needed to start native resizing (including Wayland).
        if response.drag_started_by(egui::PointerButton::Primary) {
            begin_resize(ui, direction);
        }
    }
    #[cfg(target_os = "macos")]
    continue_resize(ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_and_corners_resize_without_intercepting_content() {
        use ResizeDirection::*;
        let bounds = Rect::from_min_max(Pos2::ZERO, Pos2::new(320.0, 240.0));
        let zones = resize_zones(bounds);
        for (x, y, expected) in [
            (7.0, 100.0, West),
            (313.0, 100.0, East),
            (100.0, 7.0, North),
            (100.0, 233.0, South),
            (18.0, 7.0, NorthWest),
            (313.0, 18.0, NorthEast),
            (7.0, 222.0, SouthWest),
            (302.0, 233.0, SouthEast),
        ] {
            let hits: Vec<_> = zones
                .iter()
                .filter(|(r, _)| r.contains(Pos2::new(x, y)))
                .map(|(_, d)| *d)
                .collect();
            assert_eq!(hits, vec![expected]);
        }
        for pos in [Pos2::new(11.0, 9.0), bounds.center()] {
            assert!(zones.iter().all(|(r, _)| !r.contains(pos)));
        }
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn pressing_the_border_sends_native_resize_immediately() {
        let ctx = egui::Context::default();
        let raw = |events| egui::RawInput {
            screen_rect: Some(Rect::from_min_max(Pos2::ZERO, Pos2::new(320.0, 240.0))),
            events,
            ..Default::default()
        };
        let at = Pos2::new(6.0, 100.0);
        let _ = ctx.run_ui(raw(vec![egui::Event::PointerMoved(at)]), resize_frame);
        let output = ctx.run_ui(
            raw(vec![egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }]),
            resize_frame,
        );
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::BeginResize(ResizeDirection::West)));
    }

    #[test]
    fn manual_resize_keeps_the_opposite_corner_fixed_and_clamps_minimum_size() {
        let start = Rect::from_min_size(Pos2::new(100.0, 100.0), egui::Vec2::new(320.0, 240.0));
        let shrunk = resized_rect(start, ResizeDirection::NorthWest, egui::Vec2::splat(1000.0));
        assert_eq!(shrunk.size(), egui::Vec2::from(MIN_SIZE));
        assert_eq!(shrunk.max, start.max);
        let grown = resized_rect(
            start,
            ResizeDirection::SouthEast,
            egui::Vec2::new(80.0, 50.0),
        );
        assert_eq!(grown.min, start.min);
        assert_eq!(grown.size(), egui::Vec2::new(400.0, 290.0));
        assert_eq!(
            resized_rect(start, ResizeDirection::East, egui::Vec2::new(80.0, 50.0)).height(),
            start.height()
        );
    }
}
