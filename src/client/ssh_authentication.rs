use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use super::endpoint::{
    ClientEndpointId, ClientEndpointStatus, EndpointSupervisors, SavedSshEndpoint,
};
use super::shell::{ClientShellState, SshAuthCommand};
use super::ssh_auth::SshAuthProcess;

const INPUT_LIMIT: usize = 64 * 1024;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Authentication {
    profile: SavedSshEndpoint,
    process: Option<SshAuthProcess>,
    _transport: Option<crate::remote::SshAuthenticationCommand>,
    terminal: crate::ghostty::Terminal,
    render: crate::ghostty::RenderState,
    pending_input: VecDeque<u8>,
    size: (u16, u16),
    verifying_until: Option<Instant>,
}

fn terminal_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl Authentication {
    fn start(profile: SavedSshEndpoint, cols: u16, rows: u16) -> io::Result<Self> {
        let transport = crate::remote::ssh_authentication_command(&profile.target)?;
        let terminal =
            crate::ghostty::Terminal::new(cols.max(1), rows.max(1), 0).map_err(terminal_error)?;
        let render = crate::ghostty::RenderState::new().map_err(terminal_error)?;
        let process = SshAuthProcess::spawn(&transport.command, cols, rows)?;
        Ok(Self {
            profile,
            process: Some(process),
            _transport: Some(transport),
            terminal,
            render,
            pending_input: VecDeque::new(),
            size: (cols.max(1), rows.max(1)),
            verifying_until: None,
        })
    }

    fn endpoint_id(&self) -> ClientEndpointId {
        ClientEndpointId::Ssh(self.profile.id.clone())
    }

    fn matches_profile(&self, profiles: &[SavedSshEndpoint]) -> bool {
        profiles.iter().any(|p| {
            p.enabled
                && p.id == self.profile.id
                && p.target == self.profile.target
                && p.session == self.profile.session
        })
    }

    fn poll(&mut self, shell: &mut ClientShellState, now: Instant) -> io::Result<(bool, bool)> {
        if self.verifying_until.is_some_and(|deadline| now >= deadline) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SSH authenticated, but the machine did not reconnect. Close and try again.",
            ));
        }
        let Some(process) = self.process.as_mut() else {
            return Ok((false, false));
        };
        let size = shell.auth_popup_size().unwrap_or(self.size);
        let size = (size.0.max(1), size.1.max(1));
        let resized = size != self.size;
        if resized {
            process.resize(size.0, size.1)?;
            self.terminal
                .resize(size.0, size.1, 0, 0)
                .map_err(terminal_error)?;
            self.size = size;
        }
        while !self.pending_input.is_empty() {
            let bytes = self.pending_input.make_contiguous();
            let count = bytes.len().min(4096);
            match process.write(&bytes[..count]) {
                Ok(()) => {
                    self.pending_input.drain(..count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        let output = process.poll()?;
        let changed = resized || !output.output.is_empty();
        if changed {
            self.terminal.write(&output.output);
            // Auth terminal output must never affect the host clipboard or notifications.
            self.terminal.take_clipboard_writes();
            self.terminal.take_pwd_changes();
            self.terminal.take_bell_count();
            let text = self
                .terminal
                .read_text_viewport(
                    (0, 0),
                    (
                        size.0.saturating_sub(1),
                        u32::from(size.1.saturating_sub(1)),
                    ),
                    true,
                )
                .map_err(terminal_error)?;
            self.render.update(&self.terminal).map_err(terminal_error)?;
            let cursor = self.render.cursor().map_err(terminal_error)?;
            shell.update_auth_popup(
                text,
                cursor
                    .visible
                    .then_some(cursor.viewport)
                    .flatten()
                    .map(|p| (p.x, p.y)),
            );
        }
        match output.exit {
            Some(true) => {
                self.process.take();
                self._transport.take();
                self.pending_input.clear();
                self.verifying_until = Some(now + VERIFY_TIMEOUT);
                shell.auth_popup_verifying();
                Ok((true, true))
            }
            Some(false) => Err(io::Error::other(
                "SSH authentication did not complete. Close the popup and try again.",
            )),
            None => Ok((changed, false)),
        }
    }
}

pub(super) fn command(
    command: SshAuthCommand,
    attempt: &mut Option<Authentication>,
    profiles: &[SavedSshEndpoint],
    supervisors: &mut EndpointSupervisors,
    shell: &mut ClientShellState,
) {
    match command {
        SshAuthCommand::Start {
            endpoint_id,
            cols,
            rows,
        } => {
            if attempt.is_some() || shell.auth_popup_endpoint() != Some(&endpoint_id) {
                return;
            }
            let Some(profile) = profiles
                .iter()
                .find(|p| p.enabled && ClientEndpointId::Ssh(p.id.clone()) == endpoint_id)
            else {
                shell.auth_popup_failed("This machine was removed or disabled.".into());
                return;
            };
            if !supervisors.pause_authentication(&endpoint_id) {
                return;
            }
            match Authentication::start(profile.clone(), cols, rows) {
                Ok(started) => *attempt = Some(started),
                Err(error) => shell.auth_popup_failed(error.to_string()),
            }
        }
        SshAuthCommand::Input(bytes) => {
            if let Some(auth) = attempt.as_mut().filter(|auth| auth.process.is_some()) {
                if auth.pending_input.len().saturating_add(bytes.len()) > INPUT_LIMIT {
                    fail(
                        attempt,
                        supervisors,
                        shell,
                        "Authentication input is too large. Close and try again.".into(),
                    );
                } else {
                    auth.pending_input.extend(bytes);
                }
            }
        }
        SshAuthCommand::Cancel => {
            if let Some(auth) = attempt.take() {
                if auth.matches_profile(profiles) && auth.verifying_until.is_none() {
                    supervisors.pause_authentication(&auth.endpoint_id());
                    shell.set_endpoint_status(&auth.endpoint_id(), ClientEndpointStatus::Attention);
                }
            }
            shell.close_auth_popup();
        }
    }
}

pub(super) fn poll(
    attempt: &mut Option<Authentication>,
    profiles: &[SavedSshEndpoint],
    supervisors: &mut EndpointSupervisors,
    shell: &mut ClientShellState,
    now: Instant,
) -> bool {
    let Some(auth) = attempt.as_mut() else {
        return false;
    };
    if !auth.matches_profile(profiles) {
        // Reconciliation may already have replaced this endpoint under the same ID.
        attempt.take();
        shell.auth_popup_failed(
            "This machine changed or was removed. Close the popup and select it again.".into(),
        );
        return true;
    }
    if shell.auth_popup_endpoint() != Some(&auth.endpoint_id()) {
        if auth.verifying_until.is_none() {
            supervisors.pause_authentication(&auth.endpoint_id());
        }
        attempt.take();
        return true;
    }
    match auth.poll(shell, now) {
        Ok((changed, verify)) => {
            if verify {
                supervisors.retry_after_authentication(&auth.endpoint_id(), now);
            }
            changed
        }
        Err(error) => {
            if auth.verifying_until.is_some() {
                // Verification is only a UI deadline; background recovery still owns its retries.
                attempt.take();
                shell.auth_popup_failed(error.to_string());
            } else {
                fail(attempt, supervisors, shell, error.to_string());
            }
            true
        }
    }
}

pub(super) fn verified(
    attempt: &mut Option<Authentication>,
    endpoint_id: &ClientEndpointId,
    shell: &mut ClientShellState,
) {
    if attempt
        .as_ref()
        .is_some_and(|auth| auth.verifying_until.is_some() && &auth.endpoint_id() == endpoint_id)
    {
        attempt.take();
        shell.close_auth_popup();
    }
}

pub(super) fn rejected(
    attempt: &mut Option<Authentication>,
    endpoint_id: &ClientEndpointId,
    supervisors: &mut EndpointSupervisors,
    shell: &mut ClientShellState,
    message: &str,
) {
    if shell.endpoint_status(endpoint_id) == Some(ClientEndpointStatus::Attention)
        && attempt.as_ref().is_some_and(|auth| {
            auth.verifying_until.is_some() && &auth.endpoint_id() == endpoint_id
        })
    {
        fail(attempt, supervisors, shell, message.into());
    }
}

fn fail(
    attempt: &mut Option<Authentication>,
    supervisors: &mut EndpointSupervisors,
    shell: &mut ClientShellState,
    message: String,
) {
    if let Some(auth) = attempt.take() {
        supervisors.pause_authentication(&auth.endpoint_id());
        shell.set_endpoint_status(&auth.endpoint_id(), ClientEndpointStatus::Attention);
    }
    shell.auth_popup_failed(message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::shell::ClientShellConfig;

    // No SSH subprocess, managed config, network access, or credential prompts.
    fn fixture(
        now: Instant,
    ) -> (
        Option<Authentication>,
        SavedSshEndpoint,
        EndpointSupervisors,
        ClientShellState,
    ) {
        let profile = SavedSshEndpoint::new("Build", "build", "agents").unwrap();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let supervisors = EndpointSupervisors::new(std::slice::from_ref(&profile), now);
        let mut shell = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        shell.set_endpoint_catalog(std::slice::from_ref(&profile));
        shell.set_endpoint_status(&id, ClientEndpointStatus::Reconnecting);
        shell.open_auth_popup_for_test(id);
        shell.auth_popup_verifying();
        let attempt = Some(Authentication {
            profile: profile.clone(),
            process: None,
            _transport: None,
            terminal: crate::ghostty::Terminal::new(80, 24, 0).unwrap(),
            render: crate::ghostty::RenderState::new().unwrap(),
            pending_input: VecDeque::new(),
            size: (80, 24),
            verifying_until: Some(now + VERIFY_TIMEOUT),
        });
        (attempt, profile, supervisors, shell)
    }

    #[test]
    fn obsolete_authentication_poll_preserves_replacement_and_disabled_endpoints() {
        obsolete_authentication_preserves_endpoint(false);
    }

    #[test]
    fn obsolete_authentication_cancel_preserves_replacement_and_disabled_endpoints() {
        obsolete_authentication_preserves_endpoint(true);
    }

    fn obsolete_authentication_preserves_endpoint(cancel: bool) {
        for change in ["target", "session", "disable", "remove"] {
            let now = Instant::now();
            let (mut attempt, mut profile, mut supervisors, mut shell) = fixture(now);
            let id = ClientEndpointId::Ssh(profile.id.clone());
            match change {
                "target" => profile.target = "replacement".into(),
                "session" => profile.session = "replacement".into(),
                "disable" => profile.enabled = false,
                _ => (),
            }
            let profiles = if change == "remove" {
                vec![]
            } else {
                vec![profile]
            };
            supervisors.reconcile_profiles(&profiles, now);
            shell.retire_endpoint(&id);
            shell.set_endpoint_catalog(&profiles);
            let status = shell.endpoint_status(&id);
            let retry = supervisors.authentication_retry_for_test(&id);
            if cancel {
                command(
                    SshAuthCommand::Cancel,
                    &mut attempt,
                    &profiles,
                    &mut supervisors,
                    &mut shell,
                );
            } else {
                assert!(poll(
                    &mut attempt,
                    &profiles,
                    &mut supervisors,
                    &mut shell,
                    now
                ));
            }
            assert!(attempt.is_none());
            assert_eq!(
                shell.endpoint_status(&id),
                status,
                "{change}, cancel={cancel}"
            );
            assert_eq!(
                supervisors.authentication_retry_for_test(&id),
                retry,
                "{change}, cancel={cancel}"
            );
        }
    }

    #[test]
    fn authentication_verification_transient_error_keeps_retry_and_attempt() {
        let now = Instant::now();
        let (mut attempt, profile, mut supervisors, mut shell) = fixture(now);
        let id = ClientEndpointId::Ssh(profile.id.clone());
        rejected(
            &mut attempt,
            &id,
            &mut supervisors,
            &mut shell,
            "connection timed out",
        );
        assert!(attempt.is_some());
        assert_eq!(
            shell.endpoint_status(&id),
            Some(ClientEndpointStatus::Reconnecting)
        );
        assert_eq!(supervisors.authentication_retry_for_test(&id), Some(now));
        verified(&mut attempt, &id, &mut shell);
        assert!(attempt.is_none());
        assert!(shell.auth_popup_endpoint().is_none());
    }

    #[test]
    fn authentication_verification_cancel_preserves_background_recovery() {
        let now = Instant::now();
        let (mut attempt, profile, mut supervisors, mut shell) = fixture(now);
        let id = ClientEndpointId::Ssh(profile.id.clone());
        rejected(
            &mut attempt,
            &id,
            &mut supervisors,
            &mut shell,
            "connection timed out",
        );
        command(
            SshAuthCommand::Cancel,
            &mut attempt,
            &[profile],
            &mut supervisors,
            &mut shell,
        );
        assert!(attempt.is_none());
        assert!(shell.auth_popup_endpoint().is_none());
        assert_eq!(
            shell.endpoint_status(&id),
            Some(ClientEndpointStatus::Reconnecting)
        );
        assert_eq!(supervisors.authentication_retry_for_test(&id), Some(now));
    }

    #[test]
    fn authentication_verification_permanent_error_fails() {
        let now = Instant::now();
        let (mut attempt, profile, mut supervisors, mut shell) = fixture(now);
        let id = ClientEndpointId::Ssh(profile.id);
        shell.set_endpoint_status(&id, ClientEndpointStatus::Attention);
        rejected(
            &mut attempt,
            &id,
            &mut supervisors,
            &mut shell,
            "incompatible protocol",
        );
        assert!(attempt.is_none());
        assert_eq!(
            shell.endpoint_status(&id),
            Some(ClientEndpointStatus::Attention)
        );
        assert_eq!(supervisors.authentication_retry_for_test(&id), None);
        assert_eq!(shell.auth_popup_endpoint(), Some(&id));
    }

    #[test]
    fn authentication_verification_timeout_preserves_background_recovery() {
        let now = Instant::now();
        let (mut attempt, profile, mut supervisors, mut shell) = fixture(now);
        let id = ClientEndpointId::Ssh(profile.id.clone());
        assert!(poll(
            &mut attempt,
            &[profile],
            &mut supervisors,
            &mut shell,
            now + VERIFY_TIMEOUT
        ));
        assert!(attempt.is_none());
        assert_eq!(
            shell.endpoint_status(&id),
            Some(ClientEndpointStatus::Reconnecting)
        );
        assert_eq!(supervisors.authentication_retry_for_test(&id), Some(now));
        assert_eq!(shell.auth_popup_endpoint(), Some(&id));
        command(
            SshAuthCommand::Cancel,
            &mut attempt,
            &[],
            &mut supervisors,
            &mut shell,
        );
        assert_eq!(supervisors.authentication_retry_for_test(&id), Some(now));
    }
}
