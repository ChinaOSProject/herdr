//! Stdin input reading for the thin client.
//!
//! On Unix, reads stdin bytes and forwards framed input to the main event loop.
//! The server handles semantic parsing. On Windows, crossterm may surface
//! terminal control strings as character key events, so the reader re-frames
//! those control bytes before forwarding semantic client input events.
//!
//! This is simpler and more reliable because:
//! - The server has the same input parsing code
//! - We avoid duplicating parsing logic in the client
//! - Host terminal control replies can be buffered or discarded before they leak

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::time::Duration;
use tokio::sync::mpsc;

use super::input_epoch::InputEpoch;
use super::ClientLoopEvent;

#[cfg(any(windows, test))]
mod windows_vti;

// ---------------------------------------------------------------------------
// Stdin reader thread
// ---------------------------------------------------------------------------

/// Reads raw bytes from stdin and sends them to the main event loop.
///
/// This runs on a dedicated thread because stdin reading is blocking.
/// The main loop receives the raw bytes and forwards them as
/// `ClientMessage::Input` to the server.
pub fn stdin_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    host_color_query_sent: bool,
    host_cell_size_query_sent: bool,
    host_mouse_capture_active: Arc<AtomicBool>,
    host_sgr_pixels_active: Arc<AtomicBool>,
    host_escape_disambiguation_active: bool,
    initial_host_input: Vec<u8>,
    #[cfg(unix)] direct_response: Arc<std::sync::Mutex<super::direct_graphics::ResponseMatcher>>,
    #[cfg(unix)] direct_response_active: Arc<AtomicBool>,
    input_epoch: Arc<AtomicU64>,
) {
    #[cfg(windows)]
    {
        let _ = (
            host_color_query_sent,
            host_cell_size_query_sent,
            host_mouse_capture_active,
            host_sgr_pixels_active,
        );
        let _ = (host_escape_disambiguation_active, initial_host_input);
        windows_stdin_reader_loop(event_tx, should_quit, input_epoch);
    }

    #[cfg(unix)]
    unix_stdin_reader_loop(
        event_tx,
        should_quit,
        host_color_query_sent,
        host_cell_size_query_sent,
        host_mouse_capture_active,
        host_sgr_pixels_active,
        host_escape_disambiguation_active,
        initial_host_input,
        direct_response,
        direct_response_active,
        input_epoch,
    );
}

#[cfg(unix)]
fn unix_stdin_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    host_color_query_sent: bool,
    host_cell_size_query_sent: bool,
    host_mouse_capture_active: Arc<AtomicBool>,
    host_sgr_pixels_active: Arc<AtomicBool>,
    host_escape_disambiguation_active: bool,
    initial_host_input: Vec<u8>,
    direct_response: Arc<std::sync::Mutex<super::direct_graphics::ResponseMatcher>>,
    direct_response_active: Arc<AtomicBool>,
    input_epoch: Arc<AtomicU64>,
) {
    let stdin = io::stdin();
    unix_input_reader_loop(
        &stdin,
        event_tx,
        should_quit,
        (host_color_query_sent, host_cell_size_query_sent),
        host_mouse_capture_active,
        host_sgr_pixels_active,
        host_escape_disambiguation_active,
        initial_host_input,
        direct_response,
        direct_response_active,
        input_epoch,
    );
}

#[cfg(unix)]
fn unix_input_reader_loop<R: AsRawFd>(
    reader: &R,
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    (host_color_query_sent, host_cell_size_query_sent): (bool, bool),
    host_mouse_capture_active: Arc<AtomicBool>,
    host_sgr_pixels_active: Arc<AtomicBool>,
    host_escape_disambiguation_active: bool,
    initial_host_input: Vec<u8>,
    direct_response: Arc<std::sync::Mutex<super::direct_graphics::ResponseMatcher>>,
    direct_response_active: Arc<AtomicBool>,
    input_epoch: Arc<AtomicU64>,
) {
    let mut scratch = [0u8; 4096];
    let mut framer = unix_input_framer(
        host_escape_disambiguation_active,
        host_color_query_sent,
        host_cell_size_query_sent,
    );
    let mut pending_palette = Vec::new();
    let mut pending_mode = None;
    let mut last_geometry = None;
    let mut direct_filter = super::direct_graphics::InputFilter::default();
    let mut batch_epoch = InputEpoch(0);

    if !initial_host_input.is_empty() {
        let sgr_pixels = host_sgr_pixels_active.load(Ordering::Acquire);
        if sgr_pixels {
            last_geometry = crate::input::mouse::HostGeometry::current();
        }
        let chunks = framer.push(&initial_host_input);
        if !send_unix_input_chunks(
            chunks,
            &event_tx,
            batch_epoch,
            &mut pending_palette,
            sgr_pixels,
            last_geometry,
        ) {
            return;
        }
        if (framer.has_pending_input() || !pending_palette.is_empty())
            && stdin_read_ready(
                reader,
                idle_flush_timeout_ms(&framer, host_mouse_capture_active.load(Ordering::Acquire)),
            ) == Some(false)
        {
            let had_pending = framer.has_pending_input();
            let chunks = framer.flush_timeout();
            let held_escape = had_pending && chunks.is_empty();
            if !send_unix_input_chunks(
                chunks,
                &event_tx,
                batch_epoch,
                &mut pending_palette,
                sgr_pixels,
                last_geometry,
            ) || !flush_unix_palette_input(&event_tx, batch_epoch, &mut pending_palette)
            {
                return;
            }
            if held_escape
                && stdin_read_ready(reader, crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS)
                    == Some(false)
                && !send_unix_input_chunks(
                    framer.flush_timeout(),
                    &event_tx,
                    batch_epoch,
                    &mut pending_palette,
                    sgr_pixels,
                    last_geometry,
                )
            {
                return;
            }
        }
        pending_mode = framer.has_pending_input().then_some(sgr_pixels);
    }

    while !should_quit.load(Ordering::Acquire) {
        if direct_filter.has_pending()
            && stdin_read_ready(reader, crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS)
                == Some(false)
        {
            let released = direct_response
                .lock()
                .ok()
                .and_then(|mut matcher| direct_filter.flush_if_inactive(&mut matcher));
            if let Some(data) = released {
                if event_tx
                    .blocking_send(batch_epoch.own(ClientLoopEvent::StdinInput(data)))
                    .is_err()
                {
                    return;
                }
            }
            continue;
        }
        // Privacy boundary: readable kernel bytes keep their existing owner.
        // Adopt only a candidate captured BEFORE an observed no-data boundary;
        // otherwise a modal close between poll/read could promote queued secrets.
        let candidate = InputEpoch::capture(&input_epoch);
        match stdin_read_ready(reader, crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS) {
            Some(true) => {}
            Some(false) => {
                let next = batch_epoch.after_poll(
                    candidate,
                    false,
                    framer.has_pending_input()
                        || direct_filter.has_pending()
                        || !pending_palette.is_empty(),
                );
                if next != batch_epoch {
                    framer = unix_input_framer(
                        host_escape_disambiguation_active,
                        host_color_query_sent,
                        host_cell_size_query_sent,
                    );
                    pending_mode = None;
                    batch_epoch = next;
                }
                continue;
            }
            None => break,
        }
        // Poll and read the same kernel queue. StdinLock's 8 KiB read-ahead
        // can otherwise hide credential bytes after a 4096-byte read, making
        // an idle fd look like a safe ownership boundary before they drain.
        match crate::platform::read_fd(reader.as_raw_fd(), &mut scratch) {
            Ok(0) => break,
            Ok(n) => {
                let sgr_pixels = *pending_mode
                    .get_or_insert_with(|| host_sgr_pixels_active.load(Ordering::Acquire));
                if sgr_pixels {
                    last_geometry = retain_geometry(
                        last_geometry,
                        crate::input::mouse::HostGeometry::current(),
                    );
                }
                let filtered = filter_direct_input(
                    &scratch[..n],
                    &mut direct_filter,
                    &direct_response,
                    &direct_response_active,
                );
                let chunks = if let Some((raw_chunks, responses)) = filtered {
                    for response in responses {
                        if event_tx
                            .blocking_send(ClientLoopEvent::DirectGraphicsResponse(response))
                            .is_err()
                        {
                            return;
                        }
                    }
                    raw_chunks
                        .into_iter()
                        .flat_map(|chunk| framer.push(&chunk))
                        .collect()
                } else {
                    framer.push(&scratch[..n])
                };
                if !framer.has_pending_input() {
                    pending_mode = None;
                }
                if !send_unix_input_chunks(
                    chunks,
                    &event_tx,
                    batch_epoch,
                    &mut pending_palette,
                    sgr_pixels,
                    last_geometry,
                ) {
                    return;
                }

                let timeout_ms = idle_flush_timeout_ms(
                    &framer,
                    host_mouse_capture_active.load(Ordering::Acquire),
                );
                if stdin_read_ready(reader, timeout_ms) == Some(false) {
                    let had_pending = framer.has_pending_input();
                    let chunks = framer.flush_timeout();
                    let held_escape = had_pending && chunks.is_empty();
                    let sgr_pixels = pending_mode
                        .unwrap_or_else(|| host_sgr_pixels_active.load(Ordering::Acquire));
                    if !framer.has_pending_input() {
                        pending_mode = None;
                    }
                    if !send_unix_input_chunks(
                        chunks,
                        &event_tx,
                        batch_epoch,
                        &mut pending_palette,
                        sgr_pixels,
                        last_geometry,
                    ) || !flush_unix_palette_input(&event_tx, batch_epoch, &mut pending_palette)
                    {
                        return;
                    }
                    if held_escape
                        && stdin_read_ready(
                            reader,
                            crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS,
                        ) == Some(false)
                    {
                        let chunks = framer.flush_timeout();
                        if !framer.has_pending_input() {
                            pending_mode = None;
                        }
                        if !send_unix_input_chunks(
                            chunks,
                            &event_tx,
                            batch_epoch,
                            &mut pending_palette,
                            sgr_pixels,
                            last_geometry,
                        ) {
                            return;
                        }
                    }
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
        }
    }
}

#[cfg(unix)]
fn unix_input_framer(
    host_escape_disambiguation_active: bool,
    host_color_query_sent: bool,
    host_cell_size_query_sent: bool,
) -> crate::raw_input::RawInputByteFramer {
    let mut framer = crate::raw_input::RawInputByteFramer::for_host_input();
    framer.set_host_escape_disambiguation_active(host_escape_disambiguation_active);
    if host_color_query_sent {
        framer.host_color_query_sent();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();
    }
    if host_cell_size_query_sent {
        framer.host_cell_size_query_sent();
    }
    framer
}

#[cfg(unix)]
fn filter_direct_input(
    bytes: &[u8],
    filter: &mut super::direct_graphics::InputFilter,
    response: &std::sync::Mutex<super::direct_graphics::ResponseMatcher>,
    active: &AtomicBool,
) -> Option<(Vec<Vec<u8>>, Vec<super::direct_graphics::Response>)> {
    if !active.load(Ordering::Acquire) && !filter.has_pending() {
        return None;
    }
    Some(
        response
            .lock()
            .map(|mut matcher| filter.push(bytes, &mut matcher))
            .unwrap_or_else(|_| (vec![bytes.to_vec()], Vec::new())),
    )
}

#[cfg(unix)]
fn send_unix_input_chunks(
    chunks: Vec<Vec<u8>>,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    batch_epoch: InputEpoch,
    pending_palette: &mut Vec<Vec<u8>>,
    sgr_pixels: bool,
    geometry: Option<crate::input::mouse::HostGeometry>,
) -> bool {
    for data in chunks {
        let palette_response = std::str::from_utf8(&data)
            .ok()
            .and_then(crate::terminal_theme::parse_palette_color_response)
            .is_some();
        if palette_response {
            pending_palette.push(data);
            if pending_palette.len() == 256
                && !flush_unix_palette_input(event_tx, batch_epoch, pending_palette)
            {
                return false;
            }
            continue;
        }
        let default_color_response = std::str::from_utf8(&data)
            .ok()
            .and_then(crate::terminal_theme::parse_default_color_response)
            .is_some();
        if !default_color_response
            && !flush_unix_palette_input(event_tx, batch_epoch, pending_palette)
        {
            return false;
        }
        let Some(event) = classify_unix_input(data, sgr_pixels, geometry) else {
            continue;
        };
        if event_tx.blocking_send(batch_epoch.own(event)).is_err() {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn retain_geometry(
    last: Option<crate::input::mouse::HostGeometry>,
    observed: Option<crate::input::mouse::HostGeometry>,
) -> Option<crate::input::mouse::HostGeometry> {
    observed.or(last)
}

#[cfg(unix)]
fn classify_unix_input(
    data: Vec<u8>,
    sgr_pixels: bool,
    geometry: Option<crate::input::mouse::HostGeometry>,
) -> Option<ClientLoopEvent> {
    if sgr_pixels && crate::input::mouse::parse_report(&data).is_some() {
        return geometry.map(|geometry| ClientLoopEvent::PixelMouse(data, geometry));
    }
    Some(ClientLoopEvent::StdinInput(data))
}

#[cfg(unix)]
fn flush_unix_palette_input(
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    batch_epoch: InputEpoch,
    pending_palette: &mut Vec<Vec<u8>>,
) -> bool {
    if pending_palette.is_empty() {
        return true;
    }
    let data = std::mem::take(pending_palette).concat();
    event_tx
        .blocking_send(batch_epoch.own(ClientLoopEvent::StdinInput(data)))
        .is_ok()
}

#[cfg(unix)]
fn idle_flush_timeout_ms(
    framer: &crate::raw_input::RawInputByteFramer,
    host_mouse_capture_active: bool,
) -> i32 {
    if host_mouse_capture_active
        && (framer.has_pending_lone_escape() || framer.has_pending_incomplete_mouse_sequence())
    {
        crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS
    } else {
        crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
    }
}

#[cfg(windows)]
fn windows_stdin_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    input_epoch: Arc<AtomicU64>,
) {
    if !super::windows_vti_input_backend_enabled() {
        windows_vti::trace_input_transport("reader=crossterm");
        windows_crossterm_reader_loop(event_tx, should_quit, input_epoch);
    } else {
        match windows_vti::console_input_handle() {
            Ok(handle) => {
                windows_vti::trace_input_transport("reader=windows-console");
                windows_vti::raw_console_reader_loop(handle, event_tx, should_quit, input_epoch);
            }
            _ => {
                windows_vti::trace_input_transport("reader=crossterm-fallback");
                windows_crossterm_reader_loop(event_tx, should_quit, input_epoch);
            }
        }
    }
}

#[cfg(windows)]
fn windows_crossterm_reader_loop(
    event_tx: mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    input_epoch: Arc<AtomicU64>,
) {
    let mut framer = crate::raw_input::RawInputFramer::for_host_input();
    let mut batch_epoch = InputEpoch(0);

    while !should_quit.load(Ordering::Acquire) {
        let candidate = InputEpoch::capture(&input_epoch);
        match crossterm::event::poll(Duration::from_millis(10)) {
            Ok(true) => {}
            Ok(false) => {
                batch_epoch = batch_epoch.after_poll(candidate, false, framer.has_pending_input());
                if framer.has_pending_input() {
                    tracing::debug!("windows input raw sequence timed out; flushing");
                    if !send_windows_raw_events(framer.flush_timeout(), &event_tx, batch_epoch) {
                        return;
                    }
                }
                continue;
            }
            Err(_) => break,
        }

        // Readable input stays with the owner established at the last idle boundary.
        let event = match crossterm::event::read() {
            Ok(event) => event,
            Err(_) => break,
        };
        let (raw_events, event) = frame_windows_crossterm_event(&mut framer, event);
        if !send_windows_raw_events(raw_events, &event_tx, batch_epoch) {
            return;
        }
        let Some(event) = event else {
            continue;
        };
        if event_tx
            .blocking_send(batch_epoch.own(ClientLoopEvent::StdinEvents(vec![event])))
            .is_err()
        {
            return;
        }
    }

    if framer.has_pending_input() {
        let _ = send_windows_raw_events(framer.flush_interrupted(), &event_tx, batch_epoch);
    }
}

#[cfg(any(windows, test))]
fn frame_windows_crossterm_event(
    framer: &mut crate::raw_input::RawInputFramer,
    event: crossterm::event::Event,
) -> (
    Vec<crate::raw_input::RawInputEvent>,
    Option<crate::protocol::ClientInputEvent>,
) {
    let raw_sequence_pending = framer.has_pending_input();
    if let Some(bytes) = windows_key_raw_bytes(&event, raw_sequence_pending) {
        tracing::debug!(
            bytes = ?bytes,
            pending_before = raw_sequence_pending,
            "windows input routed through raw framer"
        );
        return (framer.push(&bytes), None);
    }

    if windows_event_is_control_key(&event) {
        tracing::debug!(event = ?event, "windows control key forwarded as semantic input");
    }
    let Some(event) = windows_crossterm_input_event(event) else {
        // Preserve the existing flush for unrelated pending input, but do not
        // cancel mouse recovery for an event that will not be forwarded.
        return (
            if raw_sequence_pending {
                framer.flush_timeout()
            } else {
                Vec::new()
            },
            None,
        );
    };
    if raw_sequence_pending {
        tracing::debug!("windows input raw sequence interrupted by semantic event; flushing");
    }
    // Even a dormant mouse prefix must stop claiming bytes after semantic input.
    (framer.flush_interrupted(), Some(event))
}

#[cfg(any(windows, test))]
fn windows_crossterm_input_event(
    event: crossterm::event::Event,
) -> Option<crate::protocol::ClientInputEvent> {
    let event = crate::protocol::ClientInputEvent::from_crossterm(event)?;
    match event {
        crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char(codepoint),
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,
            source,
            ..
        } => Some(crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char(codepoint),
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,
            repeat_count: 1,
            generated_text: Some(codepoint.to_string()),
            source,
        }),
        event => Some(event),
    }
}

#[cfg(any(windows, test))]
fn windows_event_is_control_key(event: &crossterm::event::Event) -> bool {
    use crossterm::event::{Event, KeyModifiers};

    matches!(
        event,
        Event::Key(key)
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || matches!(key.code, crossterm::event::KeyCode::Char(ch) if ch.is_control())
    )
}

#[cfg(any(windows, test))]
fn windows_key_raw_bytes(
    event: &crossterm::event::Event,
    raw_sequence_pending: bool,
) -> Option<Vec<u8>> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }

    match key.code {
        KeyCode::Esc if key.modifiers.is_empty() => Some(vec![0x1b]),
        KeyCode::Char('[') if !raw_sequence_pending && key.modifiers == KeyModifiers::CONTROL => {
            Some(vec![0x1b])
        }
        KeyCode::Char(ch)
            if !raw_sequence_pending
                && matches!(ch, 'i' | 'I')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            let mut buf = [0; 4];
            Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Char(ch) if raw_sequence_pending || ch.is_control() => {
            let mut bytes = Vec::new();
            if key.modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
            }
            let mut buf = [0; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            Some(bytes)
        }
        _ => None,
    }
}

#[cfg(windows)]
fn send_windows_raw_events(
    events: Vec<crate::raw_input::RawInputEvent>,
    event_tx: &mpsc::Sender<ClientLoopEvent>,
    batch_epoch: InputEpoch,
) -> bool {
    let raw_event_count = events.len();
    let events = events
        .into_iter()
        .filter_map(windows_client_input_event_from_raw)
        .collect::<Vec<_>>();
    if events.is_empty() {
        return true;
    }

    tracing::debug!(
        raw_event_count,
        forwarded_event_count = events.len(),
        "windows raw-framed input events forwarded"
    );
    event_tx
        .blocking_send(batch_epoch.own(ClientLoopEvent::StdinEvents(events)))
        .is_ok()
}

#[cfg(any(windows, test))]
fn windows_client_input_event_from_raw(
    event: crate::raw_input::RawInputEvent,
) -> Option<crate::protocol::ClientInputEvent> {
    match event {
        crate::raw_input::RawInputEvent::Text(text) => Some(
            crate::protocol::ClientInputEvent::TextCommit(text.into_string()),
        ),
        crate::raw_input::RawInputEvent::Key(key) => {
            let code = crate::protocol::ClientKeyCode::from_crossterm(key.code)?;
            let modifiers = key.modifiers.bits();
            let kind = crate::protocol::ClientKeyKind::from_crossterm(key.kind);
            let source = if let Some(bytes) = key.vt_bytes() {
                crate::protocol::ClientKeySource::Vt {
                    bytes: bytes.to_vec(),
                }
            } else if let Some(record) = key.windows_record() {
                crate::protocol::ClientKeySource::WindowsConsole { record }
            } else {
                crate::protocol::ClientKeySource::Synthesized
            };
            Some(crate::protocol::ClientInputEvent::Key {
                code,
                modifiers,
                kind,
                repeat_count: key.repeat_count,
                generated_text: key.generated_text.clone(),
                source,
            })
        }
        crate::raw_input::RawInputEvent::Mouse(mouse) => {
            Some(crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::from_crossterm(mouse.kind)?,
                column: mouse.column,
                row: mouse.row,
                modifiers: mouse.modifiers.bits(),
            })
        }
        crate::raw_input::RawInputEvent::Paste(text) => {
            Some(crate::protocol::ClientInputEvent::Paste { text })
        }
        crate::raw_input::RawInputEvent::OuterFocusGained => {
            Some(crate::protocol::ClientInputEvent::FocusGained)
        }
        crate::raw_input::RawInputEvent::OuterFocusLost => {
            Some(crate::protocol::ClientInputEvent::FocusLost)
        }
        crate::raw_input::RawInputEvent::HostDefaultColor { .. }
        | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
        | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
        | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
        | crate::raw_input::RawInputEvent::Unsupported => None,
    }
}

#[cfg(unix)]
fn stdin_read_ready<R: AsRawFd>(reader: &R, timeout_ms: i32) -> Option<bool> {
    poll_read_ready(reader.as_raw_fd(), timeout_ms)
}

#[cfg(unix)]
fn poll_read_ready(fd: i32, timeout_ms: i32) -> Option<bool> {
    crate::platform::poll_fd_readable(fd, timeout_ms).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn credential_pipe_larger_than_read_buffer_retains_owner_until_idle() {
        use std::io::{BufReader, Read, Write};
        use std::time::Duration;

        // Match StdinLock's read-ahead capacity, but use an isolated pipe.
        struct BufferedPipe(BufReader<io::PipeReader>);
        impl Read for BufferedPipe {
            fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
                self.0.read(data)
            }
        }
        impl AsRawFd for BufferedPipe {
            fn as_raw_fd(&self) -> std::os::fd::RawFd {
                self.0.get_ref().as_raw_fd()
            }
        }
        let (read, mut write) = io::pipe().unwrap();
        let credentials = vec![b's'; 6000];
        write.write_all(&credentials).unwrap();
        let quit = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::channel(1);
        let matcher = Arc::new(std::sync::Mutex::new(
            super::super::direct_graphics::ResponseMatcher::default(),
        ));
        let active = matcher.lock().unwrap().active_handle();
        let thread_quit = quit.clone();
        let thread_epoch = epoch.clone();
        let thread = std::thread::spawn(move || {
            unix_input_reader_loop(
                &BufferedPipe(BufReader::with_capacity(8192, read)),
                tx,
                &thread_quit,
                (false, false),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                false,
                Vec::new(),
                matcher,
                active,
                thread_epoch,
            );
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let received = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), async {
                let mut received = Vec::new();
                while received.len() < credentials.len() {
                    let event = rx.recv().await.expect("reader exited early");
                    // Closing the credential modal must not promote the tail.
                    epoch.store(1, Ordering::Release);
                    let ClientLoopEvent::OwnedInput {
                        epoch: owner,
                        event,
                    } = event
                    else {
                        panic!("unowned credential input");
                    };
                    assert_eq!(owner, 0);
                    let ClientLoopEvent::StdinInput(data) = *event else {
                        panic!("expected raw credential input");
                    };
                    received.extend(data);
                }
                // Once the pipe really drains, a new batch may adopt the
                // closed modal's epoch. Leave enough time for the idle poll.
                tokio::time::sleep(Duration::from_millis(100)).await;
                write.write_all(b"x").unwrap();
                let ClientLoopEvent::OwnedInput {
                    epoch: owner,
                    event,
                } = rx.recv().await.expect("reader exited before fresh input")
                else {
                    panic!("unowned fresh input");
                };
                assert_eq!(owner, 1);
                assert!(matches!(*event, ClientLoopEvent::StdinInput(ref data) if data == b"x"));
                received
            })
            .await
        });
        // Keep the writer open during the test: EOF is readable, not idle.
        quit.store(true, Ordering::Release);
        drop(rx);
        drop(write);
        thread.join().unwrap();
        assert_eq!(
            received.expect("credential tail stranded after false idle"),
            credentials
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdin_input_event_carries_raw_bytes() {
        let data = vec![0x1b, b'[', b'A']; // Up arrow escape sequence
        let event = ClientLoopEvent::StdinInput(data.clone());
        match event {
            ClientLoopEvent::StdinInput(d) => assert_eq!(d, data),
            _ => panic!("expected StdinInput event"),
        }
    }

    #[test]
    fn inactive_direct_input_bypasses_filter() {
        let response =
            std::sync::Mutex::new(super::super::direct_graphics::ResponseMatcher::default());
        let active = response.lock().unwrap().active_handle();
        let mut filter = super::super::direct_graphics::InputFilter::default();
        assert!(filter_direct_input(b"typed", &mut filter, &response, &active).is_none());
        assert!(!filter.has_pending());
    }

    #[test]
    fn pixel_mouse_classification_is_narrow_and_uses_read_geometry() {
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        let report = b"\x1b[<35;321;241M".to_vec();
        let Some(ClientLoopEvent::PixelMouse(data, captured)) =
            classify_unix_input(report.clone(), true, Some(geometry))
        else {
            panic!("expected dedicated pixel mouse event");
        };
        assert_eq!(data, report);
        assert_eq!(captured, geometry);
        assert!(classify_unix_input(report, true, None).is_none());

        for raw in [
            b"key".as_slice(),
            b"\x1b[200~paste\x1b[201~".as_slice(),
            b"\x1b_Gi=7;unrelated\x1b\\".as_slice(),
            b"\x1b[<35;2;3Mtail".as_slice(),
        ] {
            let Some(ClientLoopEvent::StdinInput(data)) =
                classify_unix_input(raw.to_vec(), true, Some(geometry))
            else {
                panic!("unrelated input must remain raw");
            };
            assert_eq!(data, raw);
        }
    }

    #[test]
    fn transient_geometry_failure_keeps_last_real_value() {
        let geometry = crate::input::mouse::HostGeometry::new(80, 24, 800, 480).unwrap();
        assert_eq!(retain_geometry(Some(geometry), None), Some(geometry));
    }

    #[test]
    fn close_after_read_cannot_reassign_bytes_or_delayed_paste_completion() {
        let epoch = AtomicU64::new(1);
        let mut owner = InputEpoch(1);
        let mut framer = unix_input_framer(false, false, false);
        let (tx, mut rx) = mpsc::channel(64);

        // Readiness -> stamp -> OS read -> deschedule -> modal closes.
        let read_stamp = owner.after_poll(
            InputEpoch::capture(&epoch),
            true,
            framer.has_pending_input(),
        );
        let read_bytes = b"cancel\x1b[200~secret";
        epoch.fetch_add(1, Ordering::AcqRel);
        owner = read_stamp;
        assert!(send_unix_input_chunks(
            framer.push(read_bytes),
            &tx,
            owner,
            &mut Vec::new(),
            false,
            None,
        ));
        assert!(framer.has_pending_input());

        // A later read sees the new epoch, but must finish the old paste under
        // its original owner (including any newer input in this same batch).
        owner = owner.after_poll(
            InputEpoch::capture(&epoch),
            true,
            framer.has_pending_input(),
        );
        assert!(send_unix_input_chunks(
            framer.push(b"remaining-secret\x1b[201~new input"),
            &tx,
            owner,
            &mut Vec::new(),
            false,
            None,
        ));
        let mut count = 0;
        while let Ok(event) = rx.try_recv() {
            let ClientLoopEvent::OwnedInput { epoch: stamp, .. } = event else {
                panic!("unowned auth input");
            };
            assert_eq!(stamp, 1);
            assert_ne!(stamp, epoch.load(Ordering::Acquire));
            count += 1;
        }
        assert!(count >= 2);
        assert!(!framer.has_pending_input());
        assert_eq!(
            owner.after_poll(InputEpoch::capture(&epoch), false, false),
            InputEpoch(2)
        );
    }

    #[test]
    fn epoch_transition_keeps_partial_paste_tail_with_original_owner() {
        let mut framer = unix_input_framer(false, true, true);
        let epoch = AtomicU64::new(3);
        let stamp = InputEpoch::capture(&epoch);
        assert!(framer.push(b"\x1b[200~secret").is_empty());
        epoch.fetch_add(1, Ordering::AcqRel);
        let completion = stamp.after_poll(
            InputEpoch::capture(&epoch),
            true,
            framer.has_pending_input(),
        );
        assert_eq!(completion, stamp);
        let chunks = framer.push(b"remaining-secret\x1b[201~new input");
        let (tx, mut rx) = mpsc::channel(chunks.len().max(1));
        assert!(send_unix_input_chunks(
            chunks,
            &tx,
            completion,
            &mut Vec::new(),
            false,
            None
        ));
        let mut count = 0;
        while let Ok(event) = rx.try_recv() {
            let ClientLoopEvent::OwnedInput { epoch: owner, .. } = event else {
                panic!("unowned paste tail");
            };
            assert_eq!(owner, 3);
            assert_ne!(owner, epoch.load(Ordering::Acquire));
            count += 1;
        }
        assert!(count > 0);
        assert!(!framer.has_pending_input());
        assert_eq!(
            completion.after_poll(InputEpoch::capture(&epoch), false, false),
            InputEpoch(4)
        );
    }

    #[test]
    fn queued_chunks_keep_read_epoch_after_cancel_closes_popup() {
        let epoch = AtomicU64::new(7);
        let stamp = InputEpoch::capture(&epoch);
        let (tx, mut rx) = mpsc::channel(8);
        let mut framer = crate::raw_input::RawInputByteFramer::for_host_input();
        let mut pending = Vec::new();
        assert!(send_unix_input_chunks(
            framer.push(b"\x03"),
            &tx,
            stamp,
            &mut pending,
            false,
            None,
        ));
        assert!(send_unix_input_chunks(
            framer.push(b"secret\r"),
            &tx,
            stamp,
            &mut pending,
            false,
            None,
        ));
        // Both sends predate consumption of Cancel, but are distinct events.
        epoch.fetch_add(1, Ordering::AcqRel);
        let mut count = 0;
        while let Ok(event) = rx.try_recv() {
            let ClientLoopEvent::OwnedInput { epoch: owner, .. } = event else {
                panic!("unowned input");
            };
            assert_eq!(owner, 7);
            assert_ne!(owner, epoch.load(Ordering::Acquire));
            count += 1;
        }
        assert!(count >= 2);
    }

    #[test]
    fn epoch_transition_during_blocked_batch_send_does_not_restamp_secret() {
        let epoch = Arc::new(AtomicU64::new(9));
        let stamp = InputEpoch::capture(&epoch);
        let (tx, mut rx) = mpsc::channel(1);
        let producer = std::thread::spawn(move || {
            assert!(send_unix_input_chunks(
                vec![b"\x03".to_vec(), b"secret".to_vec(), b"\r".to_vec()],
                &tx,
                stamp,
                &mut Vec::new(),
                false,
                None,
            ));
        });
        let first = rx.blocking_recv().unwrap();
        epoch.fetch_add(1, Ordering::AcqRel);
        let mut events = vec![first];
        while let Some(event) = rx.blocking_recv() {
            events.push(event);
        }
        producer.join().unwrap();
        assert_eq!(events.len(), 3);
        for event in events {
            let ClientLoopEvent::OwnedInput { epoch: owner, .. } = event else {
                panic!("unowned input");
            };
            assert_eq!(owner, 9);
            assert_ne!(owner, epoch.load(Ordering::Acquire));
        }
    }

    #[test]
    fn palette_replies_are_forwarded_as_one_input_batch() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut pending = Vec::new();
        assert!(send_unix_input_chunks(
            vec![
                b"\x1b]4;0;rgb:1111/2222/3333\x1b\\".to_vec(),
                b"\x1b]4;1;rgb:4444/5555/6666\x1b\\".to_vec(),
            ],
            &tx,
            InputEpoch(0),
            &mut pending,
            false,
            None,
        ));
        assert!(rx.try_recv().is_err());

        assert!(flush_unix_palette_input(&tx, InputEpoch(0), &mut pending));
        let ClientLoopEvent::OwnedInput { event, .. } = rx.try_recv().unwrap() else {
            panic!("expected owned palette input batch");
        };
        let ClientLoopEvent::StdinInput(data) = *event else {
            panic!("expected palette input batch");
        };
        assert_eq!(
            data.windows(4)
                .filter(|window| *window == b"\x1b]4;")
                .count(),
            2
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn raw_input_idle_flush_timeout_keeps_escape_responsive() {
        let timeout_ms = std::hint::black_box(crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS);
        assert!(timeout_ms <= 20);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_repeated_escape_keeps_second_escape_pending() {
        let mut framer = crate::raw_input::RawInputFramer::for_host_input();

        let events = framer.push(b"\x1b\x1b");

        assert_eq!(events.len(), 1);
        assert!(framer.has_pending_input());
        assert_eq!(framer.flush_timeout().len(), 1);
    }

    #[test]
    fn mouse_active_escape_sequences_get_longer_reassembly_window() {
        let mut escape = crate::raw_input::RawInputByteFramer::default();
        assert!(escape.push(b"\x1b").is_empty());
        let mut sgr_mouse = crate::raw_input::RawInputByteFramer::default();
        assert!(sgr_mouse.push(b"\x1b[<3").is_empty());
        let mut default_mouse = crate::raw_input::RawInputByteFramer::default();
        assert!(default_mouse.push(b"\x1b[MC").is_empty());
        let mut unrelated = crate::raw_input::RawInputByteFramer::default();
        assert!(unrelated.push(b"\x1b[49:33;2:").is_empty());

        for framer in [&escape, &sgr_mouse, &default_mouse, &unrelated] {
            assert_eq!(
                idle_flush_timeout_ms(framer, false),
                crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
            );
        }
        for framer in [&escape, &sgr_mouse, &default_mouse] {
            assert_eq!(
                idle_flush_timeout_ms(framer, true),
                crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS
            );
        }
        assert_eq!(
            idle_flush_timeout_ms(&unrelated, true),
            crate::raw_input::RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        );

        let mouse_timeout_ms =
            std::hint::black_box(crate::raw_input::MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS);
        assert!(mouse_timeout_ms > 100);
    }
}

#[cfg(test)]
mod windows_tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn windows_control_chars_are_reframed_as_raw_bytes() {
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(
            windows_key_raw_bytes(&escape, false).as_deref(),
            Some(b"\x1b".as_slice())
        );

        let enter = Event::Key(KeyEvent::new(KeyCode::Char('\r'), KeyModifiers::empty()));
        assert_eq!(
            windows_key_raw_bytes(&enter, false).as_deref(),
            Some(b"\r".as_slice())
        );

        let printable = Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()));
        assert_eq!(windows_key_raw_bytes(&printable, false), None);

        let pending_arrow_tail =
            Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::empty()));
        assert_eq!(
            windows_key_raw_bytes(&pending_arrow_tail, true).as_deref(),
            Some(b"[".as_slice())
        );
    }

    #[test]
    fn windows_crossterm_semantic_input_cancels_dormant_and_buffered_mouse_recovery() {
        for buffered in [false, true] {
            let mut framer = crate::raw_input::RawInputFramer::for_host_input();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.flush_timeout().is_empty());
            if buffered {
                assert!(framer.push(b"5").is_empty());
            }
            // Printable input bypasses the raw route when no bytes are pending;
            // an arrow bypasses it even when a continuation is buffered.
            let key = if buffered {
                KeyCode::Up
            } else {
                KeyCode::Char('x')
            };
            let event = Event::Key(KeyEvent::new(key, KeyModifiers::empty()));
            let (raw, semantic) = frame_windows_crossterm_event(&mut framer, event.clone());
            let raw_bytes: Vec<_> = raw
                .into_iter()
                .filter_map(|event| {
                    let crate::raw_input::RawInputEvent::Key(key) = event else {
                        return None;
                    };
                    key.vt_bytes().map(ToOwned::to_owned)
                })
                .collect();
            assert_eq!(
                raw_bytes.concat(),
                if buffered { b"5".as_slice() } else { b"" }
            );
            assert_eq!(semantic, windows_crossterm_input_event(event));
            assert_eq!(framer.push(b"5;28;31M").len(), 8);
        }
    }

    #[test]
    fn windows_crossterm_ignored_event_preserves_mouse_recovery() {
        let mut framer = crate::raw_input::RawInputFramer::for_host_input();
        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"5").is_empty());
        let (raw, semantic) = frame_windows_crossterm_event(&mut framer, Event::Resize(80, 24));
        assert!(raw.is_empty());
        assert!(semantic.is_none());
        let events = framer.push(b";28;31Mx");
        assert!(
            matches!(events.as_slice(), [crate::raw_input::RawInputEvent::Key(key)]
            if key.code == KeyCode::Char('x'))
        );
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn windows_crossterm_printable_press_keeps_key_semantics_and_text() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('你'), KeyModifiers::empty()));

        assert_eq!(
            windows_crossterm_input_event(event),
            Some(crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('你'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: Some("你".to_string()),
                source: crate::protocol::ClientKeySource::Synthesized,
            })
        );
    }

    #[test]
    fn windows_ctrl_bracket_starts_raw_escape_sequence() {
        let ctrl_bracket = Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::CONTROL));
        assert_eq!(
            windows_key_raw_bytes(&ctrl_bracket, false).as_deref(),
            Some(b"\x1b".as_slice())
        );

        let mut framer = crate::raw_input::RawInputFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[<35;48;26M");
        assert_eq!(events.len(), 1);

        let event = windows_client_input_event_from_raw(events.into_iter().next().unwrap())
            .expect("raw mouse converts");
        assert!(matches!(
            event,
            crate::protocol::ClientInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                column: 47,
                row: 25,
                modifiers: _,
            }
        ));
    }

    #[test]
    fn windows_ctrl_shift_bracket_stays_semantic() {
        let ctrl_shift_bracket = Event::Key(KeyEvent::new(
            KeyCode::Char('['),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(windows_key_raw_bytes(&ctrl_shift_bracket, false), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_ctrl_d_semantic_event_encodes_to_eot() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(windows_key_raw_bytes(&event, false), None);

        let event =
            crate::protocol::ClientInputEvent::from_crossterm(event).expect("ctrl-d converts");
        let raw = event.to_raw_input_event();
        let crate::raw_input::RawInputEvent::Key(key) = raw else {
            panic!("expected key");
        };
        assert_eq!(key.code, KeyCode::Char('d'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
        assert_eq!(
            crate::input::encode_terminal_key(key, crate::input::KeyboardProtocol::Legacy),
            b"\x04"
        );
    }

    #[test]
    fn windows_pasted_printable_ctrl_i_routes_as_literal_i() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL));
        assert_eq!(
            windows_key_raw_bytes(&event, false).as_deref(),
            Some(b"i".as_slice())
        );

        let event = Event::Key(KeyEvent::new(
            KeyCode::Char('I'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(
            windows_key_raw_bytes(&event, false).as_deref(),
            Some(b"I".as_slice())
        );
    }

    #[test]
    fn windows_eot_control_char_normalizes_to_ctrl_d() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('\u{4}'), KeyModifiers::empty()));
        let bytes = windows_key_raw_bytes(&event, false).expect("eot routes through raw framer");
        assert_eq!(bytes, b"\x04");

        let mut framer = crate::raw_input::RawInputFramer::default();
        let events = framer.push(&bytes);
        assert_eq!(events.len(), 1);

        let event = windows_client_input_event_from_raw(events.into_iter().next().unwrap())
            .expect("raw eot converts");
        assert_eq!(
            event,
            crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL.bits(),
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Vt { bytes: vec![4] },
            }
        );
    }

    #[test]
    fn windows_pending_escape_sequence_converts_to_semantic_arrow() {
        let mut framer = crate::raw_input::RawInputFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.push(b"[").is_empty());
        let events = framer.push(b"A");
        assert_eq!(events.len(), 1);

        let event = windows_client_input_event_from_raw(events.into_iter().next().unwrap())
            .expect("raw arrow converts");
        assert_eq!(
            event,
            crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Up,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Vt {
                    bytes: b"\x1b[A".to_vec()
                },
            }
        );
    }

    #[test]
    fn windows_bare_escape_flushes_to_semantic_escape() {
        let mut framer = crate::raw_input::RawInputFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.flush_timeout();
        assert_eq!(events.len(), 1);

        let event = windows_client_input_event_from_raw(events.into_iter().next().unwrap())
            .expect("raw escape converts");
        assert_eq!(
            event,
            crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Esc,
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Press,
                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Vt { bytes: vec![0x1b] },
            }
        );
    }
}
