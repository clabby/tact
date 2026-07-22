use super::{TranscriptError, record::SCHEMA_VERSION};
use crate::tui::transcript::{LocalEvent, TranscriptRecord};
use nanocodex::AgentEvent;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{sync::mpsc, task::JoinHandle};
use zstd::stream::{read::Decoder, write::Encoder};

// Journals favor low-latency ingestion; repeated JSON keys still compress well at level 1.
const COMPRESSION_LEVEL: i32 = 1;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

pub(crate) struct TranscriptJournal {
    path: PathBuf,
    sender: mpsc::UnboundedSender<Arc<TranscriptRecord>>,
    next_sequence: u64,
}

pub(crate) struct TranscriptWriter {
    task: JoinHandle<Result<(), TranscriptError>>,
}

trait DurableWrite: Write + Send + 'static {
    fn synchronize(&self) -> io::Result<()>;
}

impl DurableWrite for File {
    fn synchronize(&self) -> io::Result<()> {
        self.sync_all()
    }
}

impl TranscriptJournal {
    pub(crate) fn open(
        config_path: &Path,
        session_id: &str,
    ) -> Result<(Self, TranscriptWriter), TranscriptError> {
        let directory = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("transcripts");
        create_private_directory(&directory)?;

        let started_at = unix_milliseconds();
        let filename = format!("{started_at}-{}.jsonl.zst", sanitize_filename(session_id));
        let path = directory.join(filename);
        let file = create_private_file(&path)?;
        let (sender, receiver) = mpsc::unbounded_channel();
        let writer_path = path.clone();
        let task = tokio::task::spawn_blocking(move || write_records(file, receiver, &writer_path));

        Ok((
            Self {
                path: path.clone(),
                sender,
                next_sequence: 1,
            },
            TranscriptWriter { task },
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn append_agent(
        &mut self,
        event: AgentEvent,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        let record = TranscriptRecord::from_agent(self.take_sequence(), unix_milliseconds(), event);
        self.send(record)
    }

    pub(crate) fn append_local(
        &mut self,
        event: LocalEvent,
    ) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        let record = TranscriptRecord::from_local(self.take_sequence(), unix_milliseconds(), event)
            .map_err(|source| TranscriptError::Encode {
                path: self.path.clone(),
                source,
            })?;
        self.send(record)
    }

    fn take_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        sequence
    }

    fn send(&self, record: TranscriptRecord) -> Result<Arc<TranscriptRecord>, TranscriptError> {
        let record = Arc::new(record);
        self.sender
            .send(Arc::clone(&record))
            .map_err(|_| TranscriptError::WriterStopped(self.path.clone()))?;
        Ok(record)
    }
}

impl TranscriptWriter {
    pub(crate) fn into_task(self) -> JoinHandle<Result<(), TranscriptError>> {
        self.task
    }
}

pub(crate) fn load(path: &Path) -> Result<Vec<Arc<TranscriptRecord>>, TranscriptError> {
    let file = File::open(path).map_err(|source| TranscriptError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let decoder = Decoder::new(file).map_err(|source| TranscriptError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut input = BufReader::new(decoder);
    let mut records = Vec::new();
    let mut bytes = Vec::new();
    let mut line = 0_usize;

    loop {
        bytes.clear();
        let read = match input.read_until(b'\n', &mut bytes) {
            Ok(read) => read,
            Err(source) if incomplete_zstd_frame(&source) => break,
            Err(source) => {
                return Err(TranscriptError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        if read == 0 {
            break;
        }
        line += 1;
        if bytes.last() != Some(&b'\n') {
            break;
        }
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let record = serde_json::from_slice::<TranscriptRecord>(&bytes).map_err(|source| {
            TranscriptError::Decode {
                path: path.to_path_buf(),
                line,
                source,
            }
        })?;
        if record.schema_version() != SCHEMA_VERSION {
            return Err(TranscriptError::UnsupportedVersion {
                path: path.to_path_buf(),
                line,
                found: record.schema_version(),
                supported: SCHEMA_VERSION,
            });
        }
        let expected = u64::try_from(records.len()).unwrap_or(u64::MAX) + 1;
        if record.sequence() != expected {
            return Err(TranscriptError::Sequence {
                path: path.to_path_buf(),
                line,
                found: record.sequence(),
                expected,
            });
        }
        records.push(Arc::new(record));
    }

    Ok(records)
}

fn incomplete_zstd_frame(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::UnexpectedEof || error.to_string().contains("incomplete frame")
}

fn write_records<W: DurableWrite>(
    output: W,
    mut receiver: mpsc::UnboundedReceiver<Arc<TranscriptRecord>>,
    path: &Path,
) -> Result<(), TranscriptError> {
    let output = BufWriter::with_capacity(Encoder::<W>::recommended_input_size(), output);
    let mut output =
        Encoder::new(output, COMPRESSION_LEVEL).map_err(|source| TranscriptError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    let mut synchronized_last_record = false;
    while let Some(record) = receiver.blocking_recv() {
        serde_json::to_writer(&mut output, &record).map_err(|source| TranscriptError::Encode {
            path: path.to_path_buf(),
            source,
        })?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(|source| TranscriptError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        synchronized_last_record = record.is_sync_boundary();
        if synchronized_last_record {
            synchronize(output.get_ref().get_ref(), path)?;
        }
    }
    let mut output = output.finish().map_err(|source| TranscriptError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    output.flush().map_err(|source| TranscriptError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    if !synchronized_last_record {
        synchronize(output.get_ref(), path)?;
    }
    Ok(())
}

fn synchronize(output: &impl DurableWrite, path: &Path) -> Result<(), TranscriptError> {
    output
        .synchronize()
        .map_err(|source| TranscriptError::Sync {
            path: path.to_path_buf(),
            source,
        })
}

fn create_private_directory(path: &Path) -> Result<(), TranscriptError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(path)
        .map_err(|source| TranscriptError::CreateDirectory {
            path: path.to_path_buf(),
            source,
        })
}

fn create_private_file(path: &Path) -> Result<File, TranscriptError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .map_err(|source| TranscriptError::Create {
            path: path.to_path_buf(),
            source,
        })
}

fn sanitize_filename(session_id: &str) -> String {
    let sanitized = session_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        return "session".to_owned();
    }
    sanitized
}

fn unix_milliseconds() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{DurableWrite, TranscriptJournal, load, write_records};
    use crate::{
        config::ReasoningEffort,
        tui::transcript::{
            LocalEvent, SessionEnded, SessionOutcome, SessionStarted, TranscriptError, TurnId,
        },
    };
    use std::{
        fs,
        io::{self, Write},
        path::Path,
        sync::{Arc, Mutex, mpsc as std_mpsc},
        time::Duration,
    };
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn journal_round_trips_ordered_records_and_ignores_truncated_tail() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (mut journal, writer) = TranscriptJournal::open(&config, "session/one").unwrap();
        let path = journal.path().to_path_buf();
        journal
            .append_local(LocalEvent::SessionStarted(SessionStarted {
                session_id: "session/one".to_owned(),
                parent_session_id: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                workspace: directory.path().to_path_buf(),
                application_version: "test".to_owned(),
            }))
            .unwrap();
        journal
            .append_local(LocalEvent::UserSubmitted {
                id: TurnId::new(1),
                text: "hello".to_owned(),
            })
            .unwrap();
        journal
            .append_local(LocalEvent::SessionEnded(SessionEnded {
                outcome: SessionOutcome::Closed,
                error: None,
            }))
            .unwrap();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        let records = load(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].sequence(), 1);
        assert_eq!(records[1].kind(), "user.submitted");
        assert_eq!(records[2].kind(), "session.ended");
        assert_eq!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("zst")
        );

        let length = fs::metadata(&path).unwrap().len();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(length - 3)
            .unwrap();
        assert_eq!(load(&path).unwrap().len(), 3);
    }

    #[test]
    fn corrupt_complete_middle_line_is_rejected() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl.zst");
        let compressed = zstd::encode_all(b"not-json\n{}\n".as_slice(), 1).unwrap();
        fs::write(&path, compressed).unwrap();

        let error = load(&path).unwrap_err();
        assert!(matches!(error, TranscriptError::Decode { line: 1, .. }));
    }

    #[test]
    fn writer_flushes_every_record_and_syncs_terminal_boundary_once() {
        let state = Arc::new(Mutex::new(WriteState::default()));
        let output = RecordingWriter(Arc::clone(&state));
        let (sender, receiver) = mpsc::unbounded_channel();
        let first = super::super::record::TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::WorkerTurnAccepted { id: TurnId::new(1) },
        )
        .unwrap();
        let terminal = super::super::record::TranscriptRecord::from_local(
            2,
            2,
            LocalEvent::SessionEnded(SessionEnded {
                outcome: SessionOutcome::Closed,
                error: None,
            }),
        )
        .unwrap();
        sender.send(Arc::new(first)).unwrap();
        sender.send(Arc::new(terminal)).unwrap();
        drop(sender);

        write_records(output, receiver, Path::new("transcript.jsonl")).unwrap();

        let state = state.lock().unwrap();
        assert!(state.flushes >= 2);
        assert_eq!(state.synchronizations, 1);
        let decoded = zstd::decode_all(state.bytes.as_slice()).unwrap();
        assert_eq!(decoded.iter().filter(|&&byte| byte == b'\n').count(), 2);
    }

    #[test]
    fn flushed_records_are_readable_before_the_zstd_stream_finishes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("streaming.jsonl.zst");
        let state = Arc::new(Mutex::new(WriteState::default()));
        let (flushed, flushes) = std_mpsc::channel();
        let output = StreamingWriter {
            state: Arc::clone(&state),
            flushed,
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let record = super::super::record::TranscriptRecord::from_local(
            1,
            1,
            LocalEvent::WorkerTurnAccepted { id: TurnId::new(1) },
        )
        .unwrap();
        let writer = std::thread::spawn(move || {
            write_records(output, receiver, Path::new("streaming.jsonl.zst"))
        });

        sender.send(Arc::new(record)).unwrap();
        flushes.recv_timeout(Duration::from_secs(1)).unwrap();
        fs::write(&path, &state.lock().unwrap().bytes).unwrap();

        let records = load(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind(), "worker.turn_accepted");

        drop(sender);
        writer.join().unwrap().unwrap();
    }

    #[test]
    fn repeated_transcript_content_compresses_substantially() {
        let state = Arc::new(Mutex::new(WriteState::default()));
        let output = RecordingWriter(Arc::clone(&state));
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut uncompressed_bytes = 0;
        for sequence in 1..=50 {
            let record = super::super::record::TranscriptRecord::from_local(
                sequence,
                sequence,
                LocalEvent::UserSubmitted {
                    id: TurnId::new(sequence),
                    text: "repeated transcript content ".repeat(150),
                },
            )
            .unwrap();
            uncompressed_bytes += serde_json::to_vec(&record).unwrap().len() + 1;
            sender.send(Arc::new(record)).unwrap();
        }
        drop(sender);

        write_records(output, receiver, Path::new("compressed.jsonl.zst")).unwrap();

        let compressed_bytes = state.lock().unwrap().bytes.len();
        assert!(
            compressed_bytes * 5 < uncompressed_bytes,
            "compressed {uncompressed_bytes} bytes to {compressed_bytes} bytes"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn journal_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let (journal, writer) = TranscriptJournal::open(&config, "private").unwrap();
        let path = journal.path().to_path_buf();
        drop(journal);
        writer.into_task().await.unwrap().unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[derive(Default)]
    struct WriteState {
        bytes: Vec<u8>,
        flushes: usize,
        synchronizations: usize,
    }

    struct RecordingWriter(Arc<Mutex<WriteState>>);

    struct StreamingWriter {
        state: Arc<Mutex<WriteState>>,
        flushed: std_mpsc::Sender<()>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().flushes += 1;
            Ok(())
        }
    }

    impl DurableWrite for RecordingWriter {
        fn synchronize(&self) -> io::Result<()> {
            self.0.lock().unwrap().synchronizations += 1;
            Ok(())
        }
    }

    impl Write for StreamingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.state.lock().unwrap().bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed.send(()).map_err(io::Error::other)
        }
    }

    impl DurableWrite for StreamingWriter {
        fn synchronize(&self) -> io::Result<()> {
            Ok(())
        }
    }
}
