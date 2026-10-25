//! Delivery sinks: where a fired job's agent output lands (port of the Python
//! `scheduler_delivery.py` delivery step). The Python delivers to
//! Telegram/Discord/etc.; those platform sinks implement the same trait, so
//! they can be added without touching the engine.

use crate::jobs::DeliveryTarget;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// One delivery channel for a fired job's output.
pub trait DeliverySink: Send + Sync {
    /// Deliver one run's output. Errors fail the fire (recorded as the job's
    /// `last_error`, like a failed agent run in the Python).
    fn deliver(&self, job_name: &str, output: &str) -> Result<()>;
}

/// Deliver to the process stdout (default for `hermes cron run --foreground`).
pub struct StdoutSink {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl Default for StdoutSink {
    fn default() -> Self {
        Self { writer: Mutex::new(Box::new(std::io::stdout())) }
    }
}

impl StdoutSink {
    /// Inject a writer (tests capture into a buffer; other sinks could wrap a
    /// platform client).
    pub fn with_writer(writer: Box<dyn Write + Send>) -> Self {
        Self { writer: Mutex::new(writer) }
    }
}

impl DeliverySink for StdoutSink {
    fn deliver(&self, job_name: &str, output: &str) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        writeln!(w, "=== cron job {job_name} ===")?;
        writeln!(w, "{output}")?;
        w.flush().map_err(|e| anyhow::anyhow!("stdout delivery failed: {e}"))
    }
}

/// Append one run's output to a per-job file.
pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl DeliverySink for FileSink {
    fn deliver(&self, job_name: &str, output: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        writeln!(file, "=== cron job {job_name} ===")?;
        writeln!(file, "{output}")?;
        Ok(())
    }
}

/// Map a job's persisted delivery target to a live sink. Platform targets
/// (Telegram etc.) extend `DeliveryTarget` + this factory.
pub type SinkFactory = dyn Fn(&DeliveryTarget) -> Result<Box<dyn DeliverySink>> + Send + Sync;

pub fn default_sink_factory() -> Box<SinkFactory> {
    Box::new(|target| {
        Ok(match target {
            DeliveryTarget::Stdout => Box::new(StdoutSink::default()),
            DeliveryTarget::File { path } => Box::new(FileSink::new(path.clone())),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn stdout_sink_delivers_to_injected_writer() {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = StdoutSink::with_writer(Box::new(SharedBuf(buf.clone())));
        sink.deliver("sweeper", "all clean\nline 2").unwrap();
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("=== cron job sweeper ==="));
        assert!(text.contains("all clean\nline 2"));
    }

    #[test]
    fn file_sink_appends_per_run() {
        let dir = std::env::temp_dir().join(format!(
            "hermes-schedulers-file-sink-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let sink = FileSink::new(dir.join("out.txt"));
        sink.deliver("j", "run one").unwrap();
        sink.deliver("j", "run two").unwrap();
        let text = std::fs::read_to_string(dir.join("out.txt")).unwrap();
        assert!(text.contains("run one") && text.contains("run two"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_factory_maps_targets() {
        let factory = default_sink_factory();
        assert!(factory(&DeliveryTarget::Stdout).is_ok());
        let boxed = factory(&DeliveryTarget::File { path: PathBuf::from("x/y.txt") }).is_ok();
        assert!(boxed);
    }
}
