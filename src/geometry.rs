//! Display-independent selection geometry in logical desktop coordinates.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn between(ax: f64, ay: f64, bx: f64, by: f64) -> Self {
        Self {
            x: round_i32(ax.min(bx)),
            y: round_i32(ay.min(by)),
            width: round_u32((bx - ax).abs()),
            height: round_u32((by - ay).abs()),
        }
    }

    pub fn moved(&self, dx: f64, dy: f64, bounds_width: u32, bounds_height: u32) -> Self {
        let max_x = bounds_width.saturating_sub(self.width).min(i32::MAX as u32) as i32;
        let max_y = bounds_height
            .saturating_sub(self.height)
            .min(i32::MAX as u32) as i32;

        Self {
            x: round_i32(self.x as f64 + dx).clamp(0, max_x),
            y: round_i32(self.y as f64 + dy).clamp(0, max_y),
            width: self.width,
            height: self.height,
        }
    }

    pub fn pixels(&self, scale_x: f64, scale_y: f64) -> Self {
        // Round edges independently to avoid HiDPI gaps between rectangles.
        let left = round_i64(self.x as f64 * scale_x);
        let top = round_i64(self.y as f64 * scale_y);
        let right = round_i64((self.x as f64 + self.width as f64) * scale_x);
        let bottom = round_i64((self.y as f64 + self.height as f64) * scale_y);

        Self {
            x: clamp_i32(left),
            y: clamp_i32(top),
            width: clamp_u32(right.saturating_sub(left)),
            height: clamp_u32(bottom.saturating_sub(top)),
        }
    }

    pub fn valid(&self) -> bool {
        self.width >= 2 && self.height >= 2
    }
}

fn clamp_i32(value: i64) -> i32 {
    value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn round_i32(value: f64) -> i32 {
    value
        .round_ties_even()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn round_u32(value: f64) -> u32 {
    value.round_ties_even().clamp(0.0, u32::MAX as f64) as u32
}

fn clamp_u32(value: i64) -> u32 {
    value.clamp(0, u32::MAX as i64) as u32
}

fn round_i64(value: f64) -> i64 {
    value
        .round_ties_even()
        .clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

#[cfg(test)]
mod tests {
    use super::Rect;

    #[test]
    fn drag_in_all_directions() {
        for (start, end) in [
            ((10.0, 20.0), (110.0, 70.0)),
            ((110.0, 20.0), (10.0, 70.0)),
            ((10.0, 70.0), (110.0, 20.0)),
            ((110.0, 70.0), (10.0, 20.0)),
        ] {
            assert_eq!(
                Rect::between(start.0, start.1, end.0, end.1),
                Rect {
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 50,
                }
            );
        }
    }

    #[test]
    fn move_clamps_to_desktop_edges() {
        let rect = Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 50,
        };
        assert_eq!(
            rect.moved(-100.0, 1000.0, 500, 400),
            Rect {
                x: 0,
                y: 350,
                width: 100,
                height: 50,
            }
        );
    }

    #[test]
    fn hidpi_uses_edges_to_avoid_rounding_gaps() {
        let rect = Rect {
            x: 1,
            y: 1,
            width: 3,
            height: 3,
        };
        assert_eq!(
            rect.pixels(1.5, 1.5),
            Rect {
                x: 2,
                y: 2,
                width: 4,
                height: 4,
            }
        );
    }

    #[test]
    fn click_is_not_a_capture() {
        assert!(!Rect {
            x: 1,
            y: 1,
            width: 0,
            height: 0,
        }
        .valid());
        assert!(!Rect {
            x: 1,
            y: 1,
            width: 1,
            height: 100,
        }
        .valid());
        assert!(Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        }
        .valid());
    }

    #[test]
    fn halfway_values_use_ties_to_even() {
        assert_eq!(
            Rect::between(0.5, -0.5, 2.5, 3.5),
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 4,
            }
        );
    }

    #[test]
    fn rectangles_wider_than_the_bounds_are_pinned_at_zero() {
        let rect = Rect {
            x: 10,
            y: 20,
            width: 500,
            height: 500,
        };
        assert_eq!(rect.moved(100.0, 100.0, 100, 100).x, 0);
        assert_eq!(rect.moved(100.0, 100.0, 100, 100).y, 0);
    }
}
