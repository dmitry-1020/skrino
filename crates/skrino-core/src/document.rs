//! Document = base screenshot + ordered annotations + non-destructive crop,
//! with undo/redo.

use crate::annotation::{Annotation, Rect};
use image::RgbaImage;

/// The editing session state. Undo/redo works over snapshots of
/// (annotations, crop) — the base image never changes.
pub struct Document {
    base: RgbaImage,
    annotations: Vec<Annotation>,
    crop: Option<Rect>,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
}

#[derive(Clone)]
struct Snapshot {
    annotations: Vec<Annotation>,
    crop: Option<Rect>,
}

impl Document {
    pub fn new(base: RgbaImage) -> Self {
        Self {
            base,
            annotations: Vec::new(),
            crop: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn base(&self) -> &RgbaImage {
        &self.base
    }

    pub fn annotations(&self) -> &[Annotation] {
        &self.annotations
    }

    pub fn crop(&self) -> Option<Rect> {
        self.crop
    }

    /// Record current state to the undo stack, clearing redo.
    /// Call once before every user-visible mutation.
    pub fn push_undo(&mut self) {
        self.undo_stack.push(Snapshot {
            annotations: self.annotations.clone(),
            crop: self.crop,
        });
        self.redo_stack.clear();
    }

    pub fn add_annotation(&mut self, a: Annotation) {
        self.push_undo();
        self.annotations.push(a);
    }

    /// Mutable access for interactive editing (moving/resizing a shape).
    /// Caller is responsible for calling `push_undo` before the gesture starts.
    pub fn annotations_mut(&mut self) -> &mut Vec<Annotation> {
        &mut self.annotations
    }

    pub fn remove_annotation(&mut self, index: usize) {
        if index < self.annotations.len() {
            self.push_undo();
            self.annotations.remove(index);
        }
    }

    pub fn set_crop(&mut self, crop: Option<Rect>) {
        self.push_undo();
        self.crop = crop;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(Snapshot {
                annotations: std::mem::replace(&mut self.annotations, prev.annotations),
                crop: std::mem::replace(&mut self.crop, prev.crop),
            });
        }
    }

    pub fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(Snapshot {
                annotations: std::mem::replace(&mut self.annotations, next.annotations),
                crop: std::mem::replace(&mut self.crop, next.crop),
            });
        }
    }

    /// Next number for the Counter tool: 1 + highest existing badge number.
    pub fn next_counter_number(&self) -> u32 {
        self.annotations
            .iter()
            .filter_map(|a| match a {
                Annotation::Counter { number, .. } => Some(*number),
                _ => None,
            })
            .max()
            .map_or(1, |n| n + 1)
    }
}
