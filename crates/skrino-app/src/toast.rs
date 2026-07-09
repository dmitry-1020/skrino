//! Lightweight toast notifications: rounded cards that fade in/out at the
//! bottom-centre of the current window.

use egui::{Align2, Area, Color32, CornerRadius, FontId, Frame, Id, Order, RichText, Stroke, Vec2};

use crate::theme::Palette;

#[derive(Clone)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

/// What happens when the user clicks a toast card.
#[derive(Clone)]
pub enum ToastAction {
    None,
    /// Open this URL in the browser.
    OpenUrl(String),
    /// Re-run the last share attempt.
    Retry,
}

struct Toast {
    message: String,
    kind: ToastKind,
    created: f64,
    duration: f32,
    action: ToastAction,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub fn info(&mut self, msg: impl Into<String>) {
        self.push(msg.into(), ToastKind::Info, 3.0, ToastAction::None);
    }

    pub fn success(&mut self, msg: impl Into<String>) {
        self.push(msg.into(), ToastKind::Success, 3.0, ToastAction::None);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.push(msg.into(), ToastKind::Error, 5.0, ToastAction::None);
    }

    /// A clickable success toast that opens `url` when clicked.
    pub fn link(&mut self, msg: impl Into<String>, url: impl Into<String>) {
        self.push(
            msg.into(),
            ToastKind::Success,
            8.0,
            ToastAction::OpenUrl(url.into()),
        );
    }

    /// A clickable error toast that retries the last action when clicked.
    pub fn error_retry(&mut self, msg: impl Into<String>) {
        self.push(msg.into(), ToastKind::Error, 10.0, ToastAction::Retry);
    }

    fn push(&mut self, message: String, kind: ToastKind, duration: f32, action: ToastAction) {
        self.created_now(message, kind, duration, action);
    }

    fn created_now(&mut self, message: String, kind: ToastKind, duration: f32, action: ToastAction) {
        self.items.push(Toast {
            message,
            kind,
            created: instant_seconds(),
            duration,
            action,
        });
        // Keep the stack short.
        while self.items.len() > 4 {
            self.items.remove(0);
        }
    }

    /// Draw the toasts. Returns an action if the user clicked a toast that has
    /// one. Expired toasts are dropped.
    pub fn show(&mut self, ctx: &egui::Context, palette: &Palette) -> ToastAction {
        if self.items.is_empty() {
            return ToastAction::None;
        }

        let now = instant_seconds();
        // Drop fully-expired toasts.
        self.items
            .retain(|t| (now - t.created) as f32 <= t.duration + 0.4);

        if self.items.is_empty() {
            return ToastAction::None;
        }
        // Keep animating while visible.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));

        let mut clicked: Option<ToastAction> = None;
        let mut consumed: Option<usize> = None;

        Area::new(Id::new("skrino_toasts"))
            .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -28.0))
            .order(Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                for (idx, toast) in self.items.iter().enumerate() {
                    let elapsed = (now - toast.created) as f32;
                    let alpha = fade_alpha(elapsed, toast.duration);
                    let (accent, icon) = match toast.kind {
                        ToastKind::Info => (palette.accent, egui_phosphor::regular::INFO),
                        ToastKind::Success => {
                            (palette.success, egui_phosphor::regular::CHECK_CIRCLE)
                        }
                        ToastKind::Error => {
                            (palette.danger, egui_phosphor::regular::WARNING_CIRCLE)
                        }
                    };
                    let clickable = !matches!(toast.action, ToastAction::None);

                    let frame = Frame::new()
                        .fill(alpha_col(palette.surface, alpha))
                        .stroke(Stroke::new(1.0, alpha_col(palette.border, alpha)))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .outer_margin(egui::Margin::symmetric(0, 4));

                    let resp = frame
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(icon)
                                        .color(alpha_col(accent, alpha))
                                        .font(FontId::proportional(18.0)),
                                );
                                ui.add_space(2.0);
                                ui.label(
                                    RichText::new(&toast.message)
                                        .color(alpha_col(palette.text, alpha))
                                        .font(FontId::proportional(14.0)),
                                );
                                if matches!(toast.action, ToastAction::Retry) {
                                    ui.add_space(6.0);
                                    ui.label(
                                        RichText::new("Повторить")
                                            .color(alpha_col(accent, alpha))
                                            .font(FontId::proportional(13.0)),
                                    );
                                }
                            });
                        })
                        .response;

                    if clickable {
                        let resp = resp.interact(egui::Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            clicked = Some(toast.action.clone());
                            consumed = Some(idx);
                        }
                    }
                }
            });

        if let Some(idx) = consumed
            && idx < self.items.len() {
                self.items.remove(idx);
            }
        clicked.unwrap_or(ToastAction::None)
    }
}

/// Fade in over the first 0.15s, hold, fade out over the last 0.4s.
fn fade_alpha(elapsed: f32, duration: f32) -> f32 {
    let fade_in = (elapsed / 0.15).clamp(0.0, 1.0);
    let remaining = duration - elapsed;
    let fade_out = (remaining / 0.4).clamp(0.0, 1.0);
    fade_in.min(fade_out)
}

fn alpha_col(c: Color32, alpha: f32) -> Color32 {
    c.gamma_multiply(alpha.clamp(0.0, 1.0))
}

fn instant_seconds() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64()
}
