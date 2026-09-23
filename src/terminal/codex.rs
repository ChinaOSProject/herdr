//! Codex turn boundaries from the transcript supplied by its SessionStart hook.

use std::io::{self, BufRead};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::TerminalId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registration {
    pub terminal_id: TerminalId,
    pub generation: u64,
    pub session_id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Turn {
    pub id: String,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub registration: Registration,
    pub turn: Option<Turn>,
    pub registered_at: Instant,
    pub observed_at: Instant,
    pub replayed_idle: bool,
}

#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffSession {
    pub session_id: String,
    pub path: PathBuf,
    pub turn: Option<Turn>,
}

#[derive(Debug)]
pub(crate) struct Observation {
    pub registration: Registration,
    pub turn: Option<Turn>,
    pub observed_at: Instant,
    pub replay: bool,
    pub unavailable: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "payload")]
enum Record {
    #[serde(rename = "session_meta")]
    Session { id: String },
    #[serde(rename = "event_msg")]
    Event(Event),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "task_started")]
    Started { turn_id: String },
    #[serde(rename = "task_complete")]
    Complete { turn_id: String },
    #[serde(rename = "turn_aborted")]
    Aborted { turn_id: String },
    #[serde(other)]
    Other,
}

struct Reader {
    registration: Registration,
    file: io::BufReader<std::fs::File>,
    partial: Vec<u8>,
    consumed: u64,
    verified_session: bool,
    turn: Option<Turn>,
}

impl Reader {
    fn open(registration: Registration) -> io::Result<Self> {
        let file = std::fs::File::open(&registration.path)?;
        Ok(Self {
            registration,
            file: io::BufReader::new(file),
            partial: Vec::new(),
            consumed: 0,
            verified_session: false,
            turn: None,
        })
    }

    fn read_available(&mut self, replay: bool) -> io::Result<Vec<Turn>> {
        if std::fs::metadata(&self.registration.path)?.len() < self.consumed {
            return Err(io::Error::other("Codex transcript was truncated"));
        }
        let mut changes = Vec::new();
        loop {
            let read = self.file.read_until(b'\n', &mut self.partial)?;
            self.consumed += read as u64;
            if read == 0 || !self.partial.ends_with(b"\n") {
                break;
            }
            let record = serde_json::from_slice::<Record>(&self.partial);
            self.partial.clear();
            let next = match record {
                Ok(Record::Session { id }) => {
                    if id != self.registration.session_id {
                        return Err(io::Error::other("Codex transcript session does not match"));
                    }
                    self.verified_session = true;
                    None
                }
                Ok(Record::Event(Event::Started { turn_id })) => Some(Turn {
                    id: turn_id,
                    active: true,
                }),
                Ok(Record::Event(Event::Complete { turn_id } | Event::Aborted { turn_id }))
                    if self.turn.as_ref().is_none_or(|turn| turn.id == turn_id) =>
                {
                    Some(Turn {
                        id: turn_id,
                        active: false,
                    })
                }
                _ => None,
            };
            if let Some(next) = next.filter(|next| Some(next) != self.turn.as_ref()) {
                self.turn = Some(next.clone());
                if !replay && self.verified_session {
                    changes.push(next);
                }
            }
        }
        if replay && self.verified_session {
            changes.extend(self.turn.clone());
        }
        Ok(changes)
    }
}

pub(crate) struct Observer {
    pub registration: Registration,
    task: tokio::task::AbortHandle,
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Observer {
    pub fn start(
        registration: Registration,
        events: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    ) -> Self {
        let task_registration = registration.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = observe(task_registration.clone(), &events).await {
                tracing::warn!(path = %task_registration.path.display(), %error, "Codex turn tracking stopped");
                let _ = events
                    .send(crate::events::AppEvent::CodexTurnObserved(Observation {
                        registration: task_registration,
                        turn: None,
                        observed_at: Instant::now(),
                        replay: false,
                        unavailable: true,
                    }))
                    .await;
            }
        });
        Self {
            registration,
            task: task.abort_handle(),
        }
    }
}

async fn observe(
    registration: Registration,
    events: &tokio::sync::mpsc::Sender<crate::events::AppEvent>,
) -> io::Result<()> {
    let open_registration = registration.clone();
    let mut reader = tokio::task::spawn_blocking(move || Reader::open(open_registration)).await??;
    let mut replay = true;
    loop {
        // File work is independent of PTY parsing, screen detection, and rendering.
        let (next_reader, changes) = tokio::task::spawn_blocking(move || {
            let changes = reader.read_available(replay);
            (reader, changes)
        })
        .await?;
        reader = next_reader;
        let observed_at = Instant::now();
        for turn in changes? {
            if events
                .send(crate::events::AppEvent::CodexTurnObserved(Observation {
                    registration: registration.clone(),
                    turn: Some(turn),
                    observed_at,
                    replay,
                    unavailable: false,
                }))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
        replay = false;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture() -> Registration {
        let terminal_id = TerminalId::alloc();
        Registration {
            path: std::env::temp_dir().join(format!("herdr-codex-{terminal_id}.jsonl")),
            terminal_id,
            generation: 1,
            session_id: "session".into(),
        }
    }

    fn event(kind: &str, id: &str) -> String {
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"{kind}","turn_id":"{id}","extra":true}}}}"#
        ) + "\n"
    }

    const META: &str = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session\"}}\n";

    #[test]
    fn replay_reduces_history_and_append_matches_turn_ids_and_partial_lines() {
        let registration = fixture();
        let history = [
            META.into(),
            event("task_started", "old"),
            event("task_complete", "old"),
            event("task_started", "current"),
        ]
        .concat();
        std::fs::write(&registration.path, history).unwrap();
        let mut reader = Reader::open(registration.clone()).unwrap();
        assert_eq!(
            reader.read_available(true).unwrap(),
            vec![Turn {
                id: "current".into(),
                active: true
            }]
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&registration.path)
            .unwrap();
        file.write_all(event("task_complete", "old").as_bytes())
            .unwrap();
        let abort = event("turn_aborted", "current");
        file.write_all(&abort.as_bytes()[..abort.len() - 1])
            .unwrap();
        assert!(reader.read_available(false).unwrap().is_empty());
        file.write_all(b"\n").unwrap();
        assert_eq!(
            reader.read_available(false).unwrap(),
            vec![Turn {
                id: "current".into(),
                active: false
            }]
        );
        assert!(reader.read_available(false).unwrap().is_empty());
        drop(file);
        drop(reader);
        std::fs::remove_file(registration.path).unwrap();
    }

    #[test]
    fn empty_and_unrecognized_replay_have_no_state_and_wrong_session_is_rejected() {
        let registration = fixture();
        for contents in ["", "{\"type\":\"future_record\",\"payload\":{}}\n", META] {
            std::fs::write(&registration.path, contents).unwrap();
            let mut reader = Reader::open(registration.clone()).unwrap();
            assert!(reader.read_available(true).unwrap().is_empty());
        }
        std::fs::write(&registration.path, META.replace("session\"", "another\"")).unwrap();
        let mut reader = Reader::open(registration.clone()).unwrap();
        assert!(reader.read_available(true).is_err());
        drop(reader);
        std::fs::remove_file(registration.path).unwrap();
    }

    #[tokio::test]
    async fn observer_wakes_without_pty_output_and_releases_authority_on_read_failure() {
        let registration = fixture();
        std::fs::write(
            &registration.path,
            format!(
                "{META}{}{}",
                event("task_started", "old"),
                event("task_complete", "old")
            ),
        )
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let observer = Observer::start(registration.clone(), sender);
        async fn receive(
            receiver: &mut tokio::sync::mpsc::Receiver<crate::events::AppEvent>,
        ) -> Observation {
            match tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap()
            {
                crate::events::AppEvent::CodexTurnObserved(observation) => observation,
                _ => panic!("unexpected event"),
            }
        }
        let initial = receive(&mut receiver).await;
        assert!(initial.replay);
        assert!(!initial.turn.unwrap().active);
        assert!(receiver.try_recv().is_err());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&registration.path)
            .unwrap();
        file.write_all(event("task_started", "new").as_bytes())
            .unwrap();
        let live = receive(&mut receiver).await;
        assert!(!live.replay);
        assert!(live.turn.unwrap().active);
        drop(file);
        std::fs::write(&registration.path, "").unwrap();
        assert!(receive(&mut receiver).await.unavailable);
        drop(observer);
        std::fs::remove_file(registration.path).unwrap();
    }

    #[test]
    #[ignore = "non-gating observer filesystem scaling profile"]
    fn render_scale_profile_codex_observers() {
        for count in [1, 15] {
            let mut readers: Vec<_> = (0..count)
                .map(|_| {
                    let registration = fixture();
                    std::fs::write(
                        &registration.path,
                        format!("{META}{}", event("task_started", "turn")),
                    )
                    .unwrap();
                    let mut reader = Reader::open(registration).unwrap();
                    reader.read_available(true).unwrap();
                    reader
                })
                .collect();
            let mut samples = Vec::new();
            for _ in 0..100 {
                let start = Instant::now();
                for reader in &mut readers {
                    std::hint::black_box(reader.read_available(false).unwrap());
                }
                samples.push(start.elapsed());
            }
            samples.sort_unstable();
            println!(
                "Codex observers={count}, idle poll median={:?}, polls per second=4",
                samples[50]
            );
            for reader in readers {
                let path = reader.registration.path.clone();
                drop(reader);
                std::fs::remove_file(path).unwrap();
            }
        }
    }
}
