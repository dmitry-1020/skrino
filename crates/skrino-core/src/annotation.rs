//! Vector annotation model. All coordinates are in image pixel space
//! (physical pixels of the captured screenshot), origin top-left.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

impl Rect {
    pub fn from_points(a: Point, b: Point) -> Self {
        Self {
            min: Point::new(a.x.min(b.x), a.y.min(b.y)),
            max: Point::new(a.x.max(b.x), a.y.max(b.y)),
        }
    }

    pub fn width(&self) -> f32 {
        self.max.x - self.min.x
    }

    pub fn height(&self) -> f32 {
        self.max.y - self.min.y
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

/// Stroke style shared by drawable annotations.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub color: Color,
    /// Stroke width in image pixels.
    pub thickness: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArrowHead {
    /// Classic filled triangle head.
    Filled,
    /// Two-segment open head.
    Open,
}

/// One vector annotation on top of the screenshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Annotation {
    Arrow {
        from: Point,
        to: Point,
        head: ArrowHead,
        style: Style,
    },
    Line {
        from: Point,
        to: Point,
        style: Style,
    },
    Rect {
        rect: Rect,
        style: Style,
        /// None = outline only, Some = fill color (use alpha for translucency).
        fill: Option<Color>,
    },
    Ellipse {
        rect: Rect,
        style: Style,
        fill: Option<Color>,
    },
    Text {
        pos: Point,
        content: String,
        /// Font size in image pixels.
        size: f32,
        color: Color,
        /// Optional soft background pill behind the text for readability.
        background: Option<Color>,
    },
    /// Freehand translucent highlighter.
    Marker {
        points: Vec<Point>,
        style: Style,
    },
    /// Freehand pen stroke (opaque).
    Pen {
        points: Vec<Point>,
        style: Style,
    },
    /// Pixelate/blur a region (privacy). Rendered rasterized at export;
    /// the UI previews it live.
    Blur {
        rect: Rect,
        /// Blur strength: sigma in pixels, sensible range 4..=30.
        sigma: f32,
    },
    /// Numbered step badge (1, 2, 3, ...).
    Counter {
        pos: Point,
        number: u32,
        /// Badge radius in image pixels.
        radius: f32,
        color: Color,
    },
}

/// Active tool in the editor UI. Lives here so UI and core agree on the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    Select,
    Arrow,
    Line,
    Rect,
    Ellipse,
    Text,
    Marker,
    Pen,
    Blur,
    Counter,
    Crop,
}
