//! Placement math, ported from pypr so windows land where they did before.
//!
//! All coordinates are logical pixels (monitor size divided by scale).

use anyhow::{Result, bail};

use crate::hypr::Monitor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Animation {
    FromTop,
    FromBottom,
    FromLeft,
    FromRight,
    FromTopLeft,
    FromTopRight,
    FromBottomLeft,
    FromBottomRight,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Start,
    Center,
    End,
}

impl Animation {
    /// Case-insensitive, ignores `-`, `_` and spaces like pypr. Empty means none.
    pub fn parse(s: &str) -> Result<Option<Self>> {
        let norm: String = s.chars().filter(|c| !matches!(c, '-' | '_' | ' ')).collect::<String>().to_lowercase();
        Ok(Some(match norm.as_str() {
            "" => return Ok(None),
            "fromtop" => Self::FromTop,
            "frombottom" => Self::FromBottom,
            "fromleft" => Self::FromLeft,
            "fromright" => Self::FromRight,
            "fromtopleft" => Self::FromTopLeft,
            "fromtopright" => Self::FromTopRight,
            "frombottomleft" => Self::FromBottomLeft,
            "frombottomright" => Self::FromBottomRight,
            _ => bail!("unknown animation {s:?}"),
        }))
    }

    fn edges(self) -> (Edge, Edge) {
        use Edge::*;
        match self {
            Self::FromTop => (Center, Start),
            Self::FromBottom => (Center, End),
            Self::FromLeft => (Start, Center),
            Self::FromRight => (End, Center),
            Self::FromTopLeft => (Start, Start),
            Self::FromTopRight => (End, Start),
            Self::FromBottomLeft => (Start, End),
            Self::FromBottomRight => (End, End),
        }
    }
}

fn is_rotated(m: &Monitor) -> bool {
    matches!(m.transform, 1 | 3 | 5 | 7)
}

/// Logical (width, height) of a monitor.
pub fn logical_size(m: &Monitor) -> (i32, i32) {
    let w = (m.width as f64 / m.scale) as i32;
    let h = (m.height as f64 / m.scale) as i32;
    if is_rotated(m) { (h, w) } else { (w, h) }
}

/// `"75%"` of `reference`, `"300px"` or `"300"`.
pub fn parse_dim(s: &str, reference: i32) -> Result<i32> {
    let s = s.trim();
    if let Some(p) = s.strip_suffix('%') {
        let p: f64 = p.trim().parse()?;
        return Ok((reference as f64 * p / 100.0) as i32);
    }
    let px = s.strip_suffix("px").unwrap_or(s);
    match px.trim().parse::<i32>() {
        Ok(v) => Ok(v),
        Err(_) => bail!("bad dimension {s:?}, expected e.g. 75% or 300px"),
    }
}

/// `"X Y"` relative to the monitor's logical size.
pub fn parse_pair(s: &str, m: &Monitor) -> Result<(i32, i32)> {
    let (w, h) = logical_size(m);
    let mut it = s.split_whitespace();
    let (Some(x), Some(y), None) = (it.next(), it.next(), it.next()) else {
        bail!("expected two values like \"75% 80%\", got {s:?}");
    };
    Ok((parse_dim(x, w)?, parse_dim(y, h)?))
}

fn place(edge: Edge, mon_pos: i32, mon_len: i32, len: i32, margin: i32) -> i32 {
    match edge {
        Edge::Start => mon_pos + margin,
        Edge::Center => mon_pos + (mon_len - len) / 2,
        Edge::End => mon_pos + mon_len - len - margin,
    }
}

/// Resting on-screen position for a window of `size`.
pub fn resting(anim: Option<Animation>, m: &Monitor, size: (i32, i32), margin: i32) -> (i32, i32) {
    let (mw, mh) = logical_size(m);
    let (ex, ey) = anim.map_or((Edge::Center, Edge::Center), Animation::edges);
    (place(ex, m.x, mw, size.0, margin), place(ey, m.y, mh, size.1, margin))
}

/// Far off-screen along the animation axis, an extra monitor length away so
/// it never peeks onto a neighbouring monitor.
pub fn offscreen(anim: Animation, m: &Monitor, size: (i32, i32), margin: i32) -> (i32, i32) {
    let (rx, ry) = resting(Some(anim), m, size, margin);
    let (mw, mh) = logical_size(m);
    let (ex, ey) = anim.edges();
    let x = match ex {
        Edge::Start => m.x - size.0 - mw,
        Edge::End => m.x + mw + mw,
        Edge::Center => rx,
    };
    let y = match ey {
        Edge::Start => m.y - size.1 - mh,
        Edge::End => m.y + mh + mh,
        Edge::Center => ry,
    };
    (x, y)
}

/// Where a window at `at` slides to when hiding: one window length plus margin
/// back the way it came.
pub fn slide_out(anim: Animation, at: (i32, i32), size: (i32, i32), margin: i32) -> (i32, i32) {
    let (dx, dy) = (size.0 + margin, size.1 + margin);
    let (ex, ey) = anim.edges();
    let x = match ex {
        Edge::Start => at.0 - dx,
        Edge::End => at.0 + dx,
        Edge::Center => at.0,
    };
    let y = match ey {
        Edge::Start => at.1 - dy,
        Edge::End => at.1 + dy,
        Edge::Center => at.1,
    };
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypr::WsRef;

    /// The user's 4K monitor at 1.5x, numbers checked against pypr's debug log.
    fn dp3() -> Monitor {
        Monitor {
            id: 1,
            name: "DP-3".into(),
            x: 0,
            y: 0,
            width: 3840,
            height: 2160,
            scale: 1.5,
            transform: 0,
            focused: true,
            active_workspace: WsRef { name: "1".into() },
            special_workspace: WsRef::default(),
        }
    }

    #[test]
    fn size_and_position_match_pypr() {
        let m = dp3();
        assert_eq!(parse_pair("75% 87%", &m).unwrap(), (1920, 1252));
        assert_eq!(parse_pair("13% 7%", &m).unwrap(), (332, 100));
        assert_eq!(parse_pair("35% 50%", &m).unwrap(), (896, 720));
        assert_eq!(parse_pair("300px 200", &m).unwrap(), (300, 200));
    }

    #[test]
    fn offscreen_and_slide_match_pypr() {
        let m = dp3();
        let size = (1920, 1252);
        assert_eq!(offscreen(Animation::FromTop, &m, size, 60), (320, -2692));
        assert_eq!(slide_out(Animation::FromTop, (332, 100), size, 60), (332, -1212));
        // bluetooth: 896x720
        assert_eq!(offscreen(Animation::FromTop, &m, (896, 720), 60), (832, -2160));
        assert_eq!(slide_out(Animation::FromTop, (819, 288), (896, 720), 60), (819, -492));
    }

    #[test]
    fn resting_positions() {
        let m = dp3();
        assert_eq!(resting(Some(Animation::FromTop), &m, (1920, 1252), 60), (320, 60));
        assert_eq!(resting(Some(Animation::FromBottomRight), &m, (500, 400), 10), (2050, 1030));
        assert_eq!(resting(None, &m, (560, 440), 60), (1000, 500));
    }

    #[test]
    fn rotated_monitor_swaps_axes() {
        let mut m = dp3();
        m.transform = 1;
        assert_eq!(logical_size(&m), (1440, 2560));
    }

    #[test]
    fn animation_names_are_lenient() {
        assert_eq!(Animation::parse("fromTop").unwrap(), Some(Animation::FromTop));
        assert_eq!(Animation::parse("from-bottom_left").unwrap(), Some(Animation::FromBottomLeft));
        assert_eq!(Animation::parse("").unwrap(), None);
        assert!(Animation::parse("sideways").is_err());
    }
}
