//! skrino-core — editor model: annotations, document with undo/redo, export rendering.
//!
//! This crate is UI-agnostic: no egui types here. The UI draws annotations live
//! with its own painter; `render::render_document` produces the final raster
//! image for copy/save/upload and must visually match the UI as closely as possible.

pub mod annotation;
pub mod document;
pub mod render;

pub use annotation::{Annotation, ArrowHead, Color, Point, Rect, Style, Tool};
pub use document::Document;
