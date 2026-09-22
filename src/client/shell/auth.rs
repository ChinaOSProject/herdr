//! Client-private authentication presentation. No terminal ownership or shared popup routing.
use super::*;
use crate::raw_input::RawInputEvent;
use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

const HIDDEN_INPUT_HINT: &str = "Your input is hidden. Type your code and press Enter.";

pub(crate) enum SshAuthCommand {
    Start {
        endpoint_id: ClientEndpointId,
        cols: u16,
        rows: u16,
    },
    Input(Vec<u8>),
    Cancel,
}

impl std::fmt::Debug for SshAuthCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Start { .. } => "SshAuth::Start",
            Self::Input(_) => "SshAuth::Input([redacted])",
            Self::Cancel => "SshAuth::Cancel",
        })
    }
}

#[derive(Default)]
pub(super) struct AuthPresentation {
    required: HashSet<ClientEndpointId>,
    hover: Option<ClientEndpointId>,
    popup: Option<AuthPopup>,
}

impl AuthPresentation {
    pub(super) fn required_for(&self, endpoint: &ClientShellEndpoint) -> bool {
        !endpoint.endpoint_id.is_local()
            && endpoint.status == ClientEndpointStatus::Attention
            && self.required.contains(&endpoint.endpoint_id)
    }

    pub(super) fn badge_style(
        &self,
        endpoint: &ClientShellEndpoint,
        palette: &Palette,
        style: Style,
    ) -> Style {
        if !self.required_for(endpoint) {
            return style;
        }
        let style = style.add_modifier(Modifier::BOLD);
        if self.hover.as_ref() == Some(&endpoint.endpoint_id) {
            style
                .bg(palette.active_row_bg)
                .add_modifier(Modifier::REVERSED)
        } else {
            style
        }
    }
}

struct AuthPopup {
    endpoint: ClientEndpointId,
    text: String,
    cursor: Option<(u16, u16)>,
    phase: Phase,
    size: (u16, u16),
    cancel: Rect,
}

#[derive(PartialEq)]
enum Phase {
    Authenticating,
    Verifying,
    Failed,
}

fn safe_text(text: String) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}

fn geometry(cols: u16, rows: u16) -> (Rect, Rect, Rect) {
    let area = Rect::new(0, 0, cols, rows);
    let Some(g) = crate::popup_size::resolve_popup_geometry(
        Some(crate::popup_size::PopupSize::Percent(80)),
        Some(crate::popup_size::PopupSize::Percent(55)),
        area,
    ) else {
        return (area, area, Rect::default());
    };
    let hint_rows = Paragraph::new(HIDDEN_INPUT_HINT)
        .wrap(Wrap { trim: false })
        .line_count(g.inner.width)
        .min(usize::from(g.inner.height.saturating_sub(2))) as u16;
    let mut terminal = g.inner;
    terminal.height = terminal.height.saturating_sub(1 + hint_rows);
    let cancel = Rect::new(
        g.inner.x,
        g.inner.bottom().saturating_sub(1),
        g.inner.width.min(12),
        1,
    );
    (g.outer, terminal, cancel)
}

impl ClientShellState {
    pub(crate) fn set_endpoint_auth_required(&mut self, id: &ClientEndpointId, required: bool) {
        if required {
            self.auth.required.insert(id.clone());
        } else {
            self.auth.required.remove(id);
        }
    }

    pub(crate) fn auth_popup_endpoint(&self) -> Option<&ClientEndpointId> {
        self.auth.popup.as_ref().map(|p| &p.endpoint)
    }

    pub(crate) fn auth_popup_size(&self) -> Option<(u16, u16)> {
        self.auth.popup.as_ref().map(|p| p.size)
    }

    pub(crate) fn update_auth_popup(&mut self, text: String, cursor: Option<(u16, u16)>) {
        if let Some(p) = &mut self.auth.popup {
            if p.phase == Phase::Authenticating {
                p.text = safe_text(text);
                p.cursor = cursor;
            }
        }
    }

    pub(crate) fn auth_popup_verifying(&mut self) {
        if let Some(p) = &mut self.auth.popup {
            p.phase = Phase::Verifying;
            p.text = "Verifying connection…".into();
            p.cursor = None;
        }
    }

    pub(crate) fn auth_popup_failed(&mut self, message: String) {
        if let Some(p) = &mut self.auth.popup {
            p.phase = Phase::Failed;
            let reason: String = safe_text(message).chars().take(4096).collect();
            let keep = usize::from(p.size.1.saturating_sub(2));
            let output = p.text.trim_end();
            let mut start = output.len().saturating_sub(64 * 1024);
            while !output.is_char_boundary(start) {
                start += 1;
            }
            let output = &output[start..];
            let lines = output.lines().count();
            let tail = output
                .lines()
                .skip(lines.saturating_sub(keep))
                .collect::<Vec<_>>()
                .join("\n");
            p.text = format!("{reason}\n\n{tail}");
            p.cursor = None;
        }
    }

    pub(crate) fn close_auth_popup(&mut self) {
        self.auth.popup = None;
    }

    pub(super) fn auth_badges(&self) -> impl Iterator<Item = (Rect, &ClientEndpointId)> {
        self.hits.machines.iter().filter_map(|hit| {
            if hit.endpoint_id.is_local()
                || !self.auth.required.contains(&hit.endpoint_id)
                || self.endpoint_status(&hit.endpoint_id) != Some(ClientEndpointStatus::Attention)
                || hit.rect.width == 0
            {
                return None;
            }
            Some((hit.status_badge, &hit.endpoint_id))
        })
    }

    #[cfg(test)]
    pub(crate) fn open_auth_popup_for_test(&mut self, endpoint: ClientEndpointId) {
        self.open_auth_popup(endpoint);
    }

    pub(super) fn open_auth_popup(&mut self, endpoint: ClientEndpointId) {
        self.chrome_drag = None;
        self.workspace_press = None;
        self.tab_press = None;
        self.pane_mouse_gesture = None;
        self.link_hover = None;
        self.url_click_consumes_until_up = false;
        self.selection = None;
        self.stop_selection_autoscroll();
        self.selection_highlight_clear_deadline = None;
        self.word_selection_gesture = None;
        self.last_pane_click = None;
        self.copy_mode = None;
        self.reset_copy_pipeline();
        self.navigate_workspace_id = None;
        self.mode = ClientShellMode::Terminal;
        self.reconcile_input_source();
        self.auth.hover = None;
        let (cols, rows) = self.last_composed_size.unwrap_or((80, 24));
        let (_, inner, cancel) = geometry(cols, rows);
        self.auth.popup = Some(AuthPopup {
            endpoint,
            text: String::new(),
            cursor: None,
            phase: Phase::Authenticating,
            size: (inner.width.max(1), inner.height.max(1)),
            cancel,
        });
    }

    // A successful open consumes the remainder of this input batch.
    pub(super) fn handle_auth_badge_event(
        &mut self,
        event: &RawInputEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let RawInputEvent::Mouse(mouse) = event else {
            return false;
        };
        // Respect all existing modal/occluding chrome before looking at sidebar hits.
        let point = (mouse.column, mouse.row);
        if self.overlay.is_some()
            || self.popup_pending
            || self.popup_terminal_id.is_some()
            || self.hits.popup.is_some()
            || ((self.visible_endpoint_notice.is_some() || self.visible_notification.is_some())
                && contains(self.hits.notification_toast, point))
        {
            outcome.repaint |= self.auth.hover.take().is_some();
            return false;
        }
        let badge = self
            .auth_badges()
            .find(|(rect, _)| contains(*rect, (mouse.column, mouse.row)))
            .map(|(_, id)| id.clone());
        if mouse.kind == MouseEventKind::Moved && self.auth.hover != badge {
            self.auth.hover = badge.clone();
            outcome.repaint = true;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(endpoint_id) = badge {
                self.open_auth_popup(endpoint_id.clone());
                let Some((cols, rows)) = self.auth_popup_size() else {
                    return false;
                };
                outcome
                    .actions
                    .push(ClientShellAction::SshAuth(SshAuthCommand::Start {
                        endpoint_id,
                        cols,
                        rows,
                    }));
                outcome.repaint = true;
                return true;
            }
        }
        false
    }

    pub(super) fn handle_auth_input(&mut self, events: Vec<RawInputEvent>) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        let Some(popup) = &self.auth.popup else {
            return outcome;
        };
        let accepting = popup.phase == Phase::Authenticating;
        let mut bytes = Vec::new();
        for event in events {
            let cancel = match &event {
                RawInputEvent::Key(key) => {
                    key.code == KeyCode::Esc && key.kind != crossterm::event::KeyEventKind::Release
                }
                RawInputEvent::Mouse(mouse) => {
                    mouse.kind == MouseEventKind::Down(MouseButton::Left)
                        && contains(popup.cancel, (mouse.column, mouse.row))
                }
                _ => false,
            };
            if cancel {
                // Discard queued input too: cancellation must never replay secrets.
                outcome
                    .actions
                    .push(ClientShellAction::SshAuth(SshAuthCommand::Cancel));
                return outcome;
            }
            if accepting {
                match event {
                    RawInputEvent::Key(key) => bytes.extend(crate::input::encode_terminal_key(
                        key,
                        crate::input::KeyboardProtocol::Legacy,
                    )),
                    RawInputEvent::Text(text) => bytes.extend(text.into_string().into_bytes()),
                    RawInputEvent::Paste(text) => bytes.extend(text.into_bytes()),
                    _ => {}
                }
            }
        }
        if !bytes.is_empty() {
            outcome
                .actions
                .push(ClientShellAction::SshAuth(SshAuthCommand::Input(bytes)));
        }
        outcome
    }

    pub(super) fn auth_popup_rect(&self, cols: u16, rows: u16) -> Option<Rect> {
        self.auth.popup.as_ref().map(|_| geometry(cols, rows).0)
    }

    pub(super) fn compose_auth(&mut self, frame: &mut FrameData, cols: u16, rows: u16) {
        if self.auth.popup.is_none() {
            return;
        }
        let Some(mut buffer) = frame.to_ratatui_buffer() else {
            return;
        };
        let palette = &self.config.palette;
        let mut cursor = frame.cursor.clone();
        if let Some(p) = &mut self.auth.popup {
            let (outer, inner, cancel) = geometry(cols, rows);
            p.size = (inner.width.max(1), inner.height.max(1));
            p.cancel = cancel;
            let label = self
                .endpoints
                .iter()
                .find(|e| e.endpoint_id == p.endpoint)
                .map_or("SSH", |e| e.label.as_str());
            let phase = match p.phase {
                Phase::Authenticating => "Authentication",
                Phase::Verifying => "Verifying",
                Phase::Failed => "Failed",
            };
            Clear.render(outer, &mut buffer);
            Block::default()
                .borders(Borders::ALL)
                .title(safe_text(format!(" {label} — {phase} ")))
                .style(Style::default().fg(palette.text).bg(palette.panel_bg))
                .border_style(Style::default().fg(palette.accent))
                .render(outer, &mut buffer);
            Paragraph::new(p.text.as_str()).render(inner, &mut buffer);
            if p.phase == Phase::Authenticating {
                let hint = Rect::new(
                    inner.x,
                    inner.bottom(),
                    inner.width,
                    cancel.y.saturating_sub(inner.bottom()),
                );
                Paragraph::new(HIDDEN_INPUT_HINT)
                    .style(Style::default().fg(palette.overlay0))
                    .wrap(Wrap { trim: false })
                    .render(hint, &mut buffer);
            }
            Paragraph::new("[Cancel/Esc]")
                .style(Style::default().fg(palette.accent))
                .render(cancel, &mut buffer);
            cursor = p
                .cursor
                .filter(|(x, y)| *x < inner.width && *y < inner.height)
                .map(|(x, y)| crate::protocol::CursorState {
                    x: inner.x + x,
                    y: inner.y + y,
                    visible: true,
                    shape: 1,
                });
        }
        frame.replace_from_ratatui_buffer_preserving_effects(&buffer, cursor);
    }
}
