use super::*;
use crate::terminal::codex::{Observation, Registration, Session};

impl TerminalState {
    pub(crate) fn needs_codex_process_acquisition(&self) -> bool {
        self.detected_agent != Some(Agent::Codex) || self.recent_agent_process_exit.is_some()
    }

    #[cfg(unix)]
    pub(crate) fn handoff_codex_session(&self) -> Option<crate::terminal::codex::HandoffSession> {
        let session = self.codex_session.as_ref()?;
        self.codex_session_matches_current()
            .then(|| crate::terminal::codex::HandoffSession {
                session_id: session.registration.session_id.clone(),
                path: session.registration.path.clone(),
                turn: session.turn.clone(),
            })
    }

    #[cfg(unix)]
    pub(crate) fn restore_codex_session(
        &mut self,
        snapshot: crate::terminal::codex::HandoffSession,
    ) {
        if !self
            .persisted_agent_session
            .as_ref()
            .is_some_and(|session| {
                session.source == "herdr:codex"
                    && session.agent == "codex"
                    && session.session_ref.value == snapshot.session_id
            })
        {
            return;
        }
        self.register_codex_transcript(snapshot.path);
        if let Some(session) = self.codex_session.as_mut() {
            session.replayed_idle = snapshot.turn.as_ref().is_some_and(|turn| !turn.active);
            session.turn = snapshot.turn;
            self.detected_agent = Some(Agent::Codex);
            self.recompute_effective_state(
                self.effective_agent_label().map(str::to_string),
                self.effective_known_agent(),
                self.state,
                self.effective_presentation_for_state_at(self.state, Instant::now()),
                Instant::now(),
            );
        }
    }

    pub(crate) fn register_codex_transcript(&mut self, path: PathBuf) {
        let Some(session) = self
            .persisted_agent_session
            .as_ref()
            .filter(|session| session.source == "herdr:codex" && session.agent == "codex")
        else {
            return;
        };
        if self.recent_agent_process_exit.is_some()
            || !path.is_absolute()
            || self.codex_session.as_ref().is_some_and(|current| {
                current.registration.session_id == session.session_ref.value
                    && current.registration.path == path
            })
        {
            return;
        }
        self.codex_generation += 1;
        self.codex_session = Some(Session {
            registration: Registration {
                terminal_id: self.id.clone(),
                generation: self.codex_generation,
                session_id: session.session_ref.value.clone(),
                path,
            },
            turn: None,
            registered_at: Instant::now(),
            observed_at: Instant::now(),
            replayed_idle: false,
        });
    }

    pub(crate) fn observe_codex_turn(
        &mut self,
        observation: Observation,
    ) -> Option<TerminalStateMutation> {
        let current = self.codex_session.as_ref()?;
        if current.registration != observation.registration
            || self.recent_agent_process_exit.is_some()
            || self
                .detected_agent
                .is_some_and(|agent| agent != Agent::Codex)
            || !self.codex_session_matches_current()
        {
            return None;
        }
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let now = observation.observed_at;
        let previous_presentation = self.effective_presentation_for_state_at(previous_state, now);
        if observation.unavailable {
            self.codex_session = None;
        } else if let Some(session) = self.codex_session.as_mut() {
            session.replayed_idle =
                observation.replay && observation.turn.as_ref().is_some_and(|turn| !turn.active);
            session.turn = observation.turn;
            session.observed_at = now;
            // A live turn is new activity, not acquisition of historical state.
            if !observation.replay {
                self.agent_process_acquisition_pending = false;
            }
        }
        Some(TerminalStateMutation {
            effective_state_change: self.recompute_effective_state(
                previous_agent_label,
                previous_known_agent,
                previous_state,
                previous_presentation,
                now,
            ),
            ..Default::default()
        })
    }

    pub(super) fn codex_session_matches_current(&self) -> bool {
        self.codex_session.as_ref().is_some_and(|codex| {
            self.persisted_agent_session
                .as_ref()
                .is_some_and(|session| {
                    session.source == "herdr:codex"
                        && session.agent == "codex"
                        && session.session_ref.value == codex.registration.session_id
                })
        })
    }

    pub(super) fn codex_state(&self) -> Option<AgentState> {
        if self.detected_agent != Some(Agent::Codex) || !self.codex_session_matches_current() {
            return None;
        }
        let session = self.codex_session.as_ref()?;
        let turn = session.turn.as_ref()?;
        Some(if self.fallback_visible_blocker {
            AgentState::Blocked
        } else if turn.active
            || (self.fallback_visible_working
                && self
                    .fallback_observed_at
                    .is_some_and(|at| at > session.observed_at))
        {
            AgentState::Working
        } else {
            AgentState::Idle
        })
    }

    pub(crate) fn observe_codex_prompt(
        &mut self,
        observed_at: Instant,
    ) -> Option<TerminalStateMutation> {
        if self.detected_agent != Some(Agent::Codex) || self.recent_agent_process_exit.is_some() {
            return None;
        }
        if self
            .codex_prompt_started_at
            .is_some_and(|started| observed_at < started)
        {
            return None;
        }
        self.codex_prompt_ready = true;
        Some(TerminalStateMutation::default())
    }

    pub(super) fn managed_agent_prompt_ready(&self, agent: Agent) -> bool {
        self.state == AgentState::Idle
            || (agent == Agent::Codex
                && self.codex_prompt_ready
                && self.state == AgentState::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::codex::Turn;

    fn register(terminal: &mut TerminalState, id: &str) -> Registration {
        terminal
            .set_agent_session_ref_for_session_start(
                "herdr:codex".into(),
                "codex".into(),
                crate::agent_resume::AgentSessionRef::id(id),
                None,
                Some("startup".into()),
            )
            .unwrap();
        terminal.register_codex_transcript(std::env::temp_dir().join(format!("{id}.jsonl")));
        terminal
            .codex_session
            .as_ref()
            .unwrap()
            .registration
            .clone()
    }

    fn observe(
        terminal: &mut TerminalState,
        registration: &Registration,
        active: bool,
        replay: bool,
        at: Instant,
    ) {
        terminal
            .observe_codex_turn(Observation {
                registration: registration.clone(),
                turn: Some(Turn {
                    id: "turn".into(),
                    active,
                }),
                observed_at: at,
                replay,
                unavailable: false,
            })
            .unwrap();
    }

    fn screen(terminal: &mut TerminalState, state: AgentState, at: Instant) {
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            state,
            state == AgentState::Blocked,
            false,
            state == AgentState::Working,
            false,
            at,
        );
    }

    #[test]
    fn codex_turn_owns_ambiguous_screen_and_preserves_blockers_and_event_order() {
        let mut terminal = TerminalState::new(TerminalId::alloc(), std::env::temp_dir());
        let at = Instant::now();
        // SessionStart can precede process detection and must survive an unknown scan.
        let registration = register(&mut terminal, "session");
        terminal.set_detected_state(None, AgentState::Unknown);
        observe(&mut terminal, &registration, true, true, at);
        screen(&mut terminal, AgentState::Unknown, at);
        assert_eq!(terminal.state, AgentState::Working);
        screen(&mut terminal, AgentState::Blocked, at);
        observe(
            &mut terminal,
            &registration,
            false,
            true,
            at + Duration::from_millis(2),
        );
        assert_eq!(terminal.state, AgentState::Blocked);
        screen(
            &mut terminal,
            AgentState::Unknown,
            at + Duration::from_millis(3),
        );
        assert_eq!(terminal.state, AgentState::Idle);
        assert!(terminal.finish_agent_process_acquisition());
        // An older queued screen event cannot undo completion.
        screen(
            &mut terminal,
            AgentState::Working,
            at + Duration::from_millis(1),
        );
        assert_eq!(terminal.state, AgentState::Idle);
        screen(
            &mut terminal,
            AgentState::Working,
            at + Duration::from_millis(4),
        );
        assert_eq!(terminal.state, AgentState::Working);
        observe(
            &mut terminal,
            &registration,
            false,
            false,
            at + Duration::from_millis(5),
        );
        assert_eq!(terminal.state, AgentState::Idle);
        assert!(!terminal.finish_agent_process_acquisition());
        let mut terminal = TerminalState::new(TerminalId::alloc(), std::env::temp_dir());
        screen(&mut terminal, AgentState::Unknown, at);
        terminal.set_hook_authority_at(
            "custom:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            None,
            None,
            at + Duration::from_millis(6),
        );
        assert_eq!(terminal.state, AgentState::Working);
        let registration = register(&mut terminal, "session");
        observe(
            &mut terminal,
            &registration,
            false,
            true,
            at + Duration::from_millis(7),
        );
        assert_eq!(terminal.state, AgentState::Working);
        screen(
            &mut terminal,
            AgentState::Blocked,
            at + Duration::from_millis(7),
        );
        assert_eq!(terminal.state, AgentState::Blocked);
    }

    #[test]
    fn codex_registration_replacement_exit_and_failure_discard_stale_authority() {
        let mut terminal = TerminalState::new(TerminalId::alloc(), std::env::temp_dir());
        let at = Instant::now();
        screen(&mut terminal, AgentState::Unknown, at);
        let old = register(&mut terminal, "old");
        observe(&mut terminal, &old, true, true, at);
        assert_eq!(
            register(&mut terminal, "old"),
            old,
            "compaction does not restart replay"
        );
        let current = register(&mut terminal, "new");
        assert_ne!(old.generation, current.generation);
        assert!(terminal
            .observe_codex_turn(Observation {
                registration: old,
                turn: None,
                observed_at: at,
                replay: false,
                unavailable: true
            })
            .is_none());
        observe(&mut terminal, &current, false, true, at);
        screen(
            &mut terminal,
            AgentState::Working,
            at - Duration::from_millis(1),
        );
        assert_eq!(terminal.state, AgentState::Idle);
        terminal
            .observe_codex_turn(Observation {
                registration: current.clone(),
                turn: None,
                observed_at: at + Duration::from_millis(2),
                replay: false,
                unavailable: true,
            })
            .unwrap();
        assert_eq!(terminal.state, AgentState::Working);
        assert!(terminal.codex_session.is_none());
        let current = register(&mut terminal, "new");
        assert!(current.generation > 1);
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Unknown,
            false,
            false,
            false,
            true,
            at + Duration::from_millis(3),
        );
        assert!(terminal.codex_session.is_none());
        // A legacy identity report still cannot restore transcript authority after exit.
        terminal.set_agent_session_ref_for_session_start(
            "herdr:codex".into(),
            "codex".into(),
            crate::agent_resume::AgentSessionRef::id("new"),
            None,
            None,
        );
        terminal.register_codex_transcript(std::env::temp_dir().join("new.jsonl"));
        assert!(terminal.codex_session.is_none());
        // Guarded admission can acquire a verified replacement before the detector ticks.
        assert!(terminal.needs_codex_process_acquisition());
        terminal.set_detected_agent_process_at(Agent::Codex, at + Duration::from_millis(4));
        let replacement = register(&mut terminal, "replacement");
        assert!(!terminal.needs_codex_process_acquisition());
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Unknown,
            false,
            false,
            false,
            true,
            at - Duration::from_millis(1),
        );
        assert_eq!(
            terminal.codex_session.as_ref().unwrap().registration,
            replacement
        );
        terminal.clear_agent_runtime_identity_after_respawn();
        let replacement = register(&mut terminal, "new");
        assert!(replacement.generation > current.generation);
    }

    #[test]
    fn codex_managed_startup_requires_fresh_prompt_evidence_not_unknown_or_blocked() {
        let mut terminal = TerminalState::new(TerminalId::alloc(), std::env::temp_dir());
        let at = Instant::now();
        terminal.begin_managed_agent(
            "worker".into(),
            Agent::Codex,
            at,
            Duration::ZERO,
            Duration::from_secs(60),
        );
        screen(&mut terminal, AgentState::Unknown, at);
        terminal.reconcile_managed_agent_at(at, false);
        assert!(!terminal.managed_agent_interactive_ready());
        assert!(terminal
            .observe_codex_prompt(at - Duration::from_millis(1))
            .is_none());
        terminal.observe_codex_prompt(at).unwrap();
        screen(&mut terminal, AgentState::Blocked, at);
        terminal.reconcile_managed_agent_at(at, false);
        assert!(!terminal.managed_agent_interactive_ready());
        screen(&mut terminal, AgentState::Unknown, at);
        terminal.reconcile_managed_agent_at(at, false);
        assert!(terminal.managed_agent_interactive_ready());
        assert_eq!(terminal.state, AgentState::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn codex_handoff_rebinds_observer_to_new_terminal_identity() {
        let mut original = TerminalState::new(TerminalId::alloc(), std::env::temp_dir());
        screen(&mut original, AgentState::Unknown, Instant::now());
        let old = register(&mut original, "session");
        observe(&mut original, &old, true, false, Instant::now());
        let mut restored = TerminalState::new(TerminalId::alloc(), std::env::temp_dir());
        restored.persisted_agent_session = original.persisted_agent_session.clone();
        restored.restore_codex_session(original.handoff_codex_session().unwrap());
        let registration = &restored.codex_session.as_ref().unwrap().registration;
        assert_eq!(registration.terminal_id, restored.id);
        assert_ne!(registration.terminal_id, old.terminal_id);
        assert_eq!(restored.state, AgentState::Working);
        assert!(restored
            .observe_codex_turn(Observation {
                registration: old,
                turn: None,
                observed_at: Instant::now(),
                replay: false,
                unavailable: true
            })
            .is_none());
    }
}
