use rune::{Display, WindowPlacement};

use super::document::{Bounds, Placement};

#[derive(Clone, Copy)]
pub(super) struct Monitor {
    position: (i32, i32),
    size: (u32, u32),
    scale: f64,
    logical: [f64; 4],
}

impl Monitor {
    pub(super) fn new(display: &Display) -> Self {
        let scale = display.scale;
        let logical = if cfg!(target_os = "macos") {
            display.native_bounds()
        } else {
            [
                display.position.0 as f64,
                display.position.1 as f64,
                display.size.0 as f64,
                display.size.1 as f64,
            ]
            .map(|value| value / scale)
        };
        Self {
            position: display.position,
            size: display.size,
            scale,
            logical,
        }
    }

    pub(super) fn restore(self, bounds: Bounds) -> Option<((i32, i32), (u32, u32))> {
        if !bounds.fits(self.logical) {
            return None;
        }
        let x = self.position.0 as f64 + (bounds.x as f64 - self.logical[0]) * self.scale;
        let y = self.position.1 as f64 + (bounds.y as f64 - self.logical[1]) * self.scale;
        let width = bounds.width as f64 * self.scale;
        let height = bounds.height as f64 * self.scale;
        Some((
            (x.round() as i32, y.round() as i32),
            (width.round() as u32, height.round() as u32),
        ))
    }

    fn for_window(self, placement: WindowPlacement) -> bool {
        placement.display == Some((self.position, self.size))
    }

    fn logical_bounds(self, position: (i32, i32), size: (u32, u32), scale: f64) -> Option<Bounds> {
        let x = self.logical[0] + (position.0 as f64 - self.position.0 as f64) / scale;
        let y = self.logical[1] + (position.1 as f64 - self.position.1 as f64) / scale;
        Bounds::new(
            x.round() as i64,
            y.round() as i64,
            (size.0 as f64 / scale).round() as i64,
            (size.1 as f64 / scale).round() as i64,
        )
    }

    fn overlap(self, bounds: Bounds) -> f64 {
        let [x, y, width, height] = self.logical;
        let horizontal =
            (x + width).min(bounds.x as f64 + bounds.width as f64) - x.max(bounds.x as f64);
        let vertical =
            (y + height).min(bounds.y as f64 + bounds.height as f64) - y.max(bounds.y as f64);
        horizontal.max(0.0) * vertical.max(0.0)
    }

    fn adjust_normal(self, bounds: Bounds, monitors: &[Monitor]) -> Bounds {
        if bounds.fits(self.logical) {
            return bounds;
        }
        // Equal overlap uses the first display in the desktop's enumeration.
        let source = monitors
            .iter()
            .rev()
            .copied()
            .filter(|monitor| monitor.overlap(bounds) > 0.0)
            .max_by(|left, right| left.overlap(bounds).total_cmp(&right.overlap(bounds)))
            .unwrap_or(self);
        let [x, y, width, height] = self.logical;
        let new_width = width.floor().min(bounds.width as f64) as i64;
        let new_height = height.floor().min(bounds.height as f64) as i64;
        let new_x =
            (x + bounds.x as f64 - source.logical[0]).clamp(x, x + width - new_width as f64);
        let new_y =
            (y + bounds.y as f64 - source.logical[1]).clamp(y, y + height - new_height as f64);
        Bounds::new(
            new_x.round() as i64,
            new_y.round() as i64,
            new_width,
            new_height,
        )
        .unwrap_or(bounds)
    }
}

pub(super) fn capture(
    placement: WindowPlacement,
    previous_normal: Option<Bounds>,
    monitors: &[Monitor],
) -> Option<Placement> {
    if placement.minimized || !placement.scale.is_finite() || placement.scale <= 0.0 {
        return None;
    }
    let target = monitors
        .iter()
        .copied()
        .find(|monitor| monitor.for_window(placement));
    let decode = |position: (i32, i32), size: (u32, u32)| {
        let scale = placement.scale;
        if let Some(monitor) = target {
            return monitor.logical_bounds(position, size, scale);
        }
        Bounds::new(
            (position.0 as f64 / scale).round() as i64,
            (position.1 as f64 / scale).round() as i64,
            (size.0 as f64 / scale).round() as i64,
            (size.1 as f64 / scale).round() as i64,
        )
    };
    let bounds = if placement.maximized || placement.full_screen {
        let normal = placement
            .restore
            .and_then(|(position, size)| decode(position, size))
            .or(previous_normal)?;
        target.map_or(normal, |monitor| monitor.adjust_normal(normal, monitors))
    } else {
        decode(placement.position, placement.content_size)?
    };
    Some(Placement {
        bounds,
        maximized: placement.maximized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximized_window_uses_first_display_for_an_equal_overlap_restore_frame() {
        let left = Monitor {
            position: (0, 0),
            size: (1600, 1100),
            scale: 1.0,
            logical: [0.0, 0.0, 1600.0, 1100.0],
        };
        let right = Monitor {
            position: (1600, 0),
            logical: [1600.0, 0.0, 1600.0, 1100.0],
            ..left
        };
        let normal = Bounds::new(1140, 96, 920, 700).unwrap();
        let maximized = WindowPlacement {
            position: right.position,
            content_size: right.size,
            scale: 1.0,
            maximized: true,
            display: Some((right.position, right.size)),
            ..WindowPlacement::default()
        };

        let restored = capture(maximized, Some(normal), &[left, right]).unwrap();
        assert_eq!(restored.bounds, Bounds::new(2280, 96, 920, 700).unwrap());
        assert!(restored.maximized);
    }
}
