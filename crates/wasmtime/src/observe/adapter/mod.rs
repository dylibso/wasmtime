use crate::Linker;
use anyhow::Result;
use log::warn;
use std::sync::mpsc::{channel, Sender};

use crate::observe::{
    collector::{Collector, CollectorHandle},
    context::add_to_linker,
    Event, TelemetryId, TraceEvent,
};

use self::datadog::DatadogMetadata;

pub mod datadog;
pub mod datadog_formatter;
pub mod otel_formatter;
pub mod otelstdout;
pub mod stdout;
pub mod zipkin;
pub mod zipkin_formatter;

/// An adapter represents a sink for events and is mostly implementation specific
/// to the sink that the data is being sent to and the format that the data is in.
/// Collectors batch up events and metadata into a TraceEvent and then sends this
/// payload to the Adapter when the module is done running. When implementing an adapter,
/// your job is to handle this event by implementing the function handle_trace_event.
///
/// ┌────────────────┐               ┌──────────────┐                ┌────────────────┐
/// │   Collector    │               │    Adapter   │                │   Collector    │
/// │                │               │              │                │                │
/// │                │               │              │                │                │
/// ├────────────────┤  <TraceEvent> ├──────────────┤  <TraceEvent>  ├────────────────┤
/// │  adapter_tx    ├──────────────►│ adapter_rx   │◄───────────────┤  adapter_tx    │
/// └────────────────┘               └──────────────┘                └────────────────┘
pub trait Adapter {
    /// Callback which is used when a TraceEvent is sent from the Collector
    /// This must be implemented by an Adapter
    fn handle_trace_event(&mut self, evt: TraceEvent) -> Result<()>;

    /// Spawns the tokio task and returns a cloneable handle to this adapter
    fn spawn(mut adapter: impl Adapter + Send + 'static) -> AdapterHandle {
        let (adapter_tx, adapter_rx) = channel::<TraceEvent>();
        let done = std::sync::Arc::new(std::sync::Mutex::new(false));
        let d = done.clone();
        std::thread::spawn(move || loop {
            *d.lock().unwrap() = false;
            if let Ok(evt) = adapter_rx.recv() {
                if let Err(e) = adapter.handle_trace_event(evt) {
                    warn!("Error handling events in adapter {e}");
                }
                *d.lock().unwrap() = true;
            }
        });
        AdapterHandle {
            adapter_tx,
            done,
            shutdown: std::sync::Arc::new(std::sync::Mutex::new(false)),
        }
    }
}

/// Represents handle into the trace that is currently executing.
#[derive(Clone, Debug)]
pub struct TraceContext {
    collector: CollectorHandle,

    pub(crate) join_handle: std::sync::Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>>,

    pub(crate) shutdown: std::sync::Arc<std::sync::Mutex<bool>>,
}
unsafe impl Sync for TraceContext {}
unsafe impl Send for TraceContext {}

impl TraceContext {
    pub fn set_trace_id(&self, id: TelemetryId) {
        if let Err(e) = self.collector.send(Event::TraceId(id)) {
            warn!("Failed to set the trace id {}", e);
        }
    }
    pub fn set_metadata(&self, meta: AdapterMetadata) {
        if let Err(e) = self.collector.send(Event::Metadata(meta)) {
            warn!("Failed to set the metdata {}", e);
        }
    }

    pub fn shutdown(&self) {
        if let Err(e) = self.collector.send(Event::Shutdown) {
            warn!("Failed to shutdown collector {}", e);
        }

        if let Ok(mut lock) = self.shutdown.lock() {
            *lock = true;
        }

        if let Ok(mut lock) = self.join_handle.lock() {
            if let Some(x) = lock.take() {
                let _ = x.join();
            }
        }
    }
}

/// Represents a cloneable handle to the Adapter. Calling start gives
/// you a TraceContext that is linked to the Wasm module.
#[derive(Clone, Debug)]
pub struct AdapterHandle {
    adapter_tx: Sender<TraceEvent>,

    pub(crate) done: std::sync::Arc<std::sync::Mutex<bool>>,

    pub(crate) shutdown: std::sync::Arc<std::sync::Mutex<bool>>,
}

unsafe impl Sync for AdapterHandle {}
unsafe impl Send for AdapterHandle {}

impl AdapterHandle {
    pub fn start<T>(&self, linker: &mut Linker<T>, data: &[u8]) -> Result<TraceContext> {
        let (collector, collector_rx) = add_to_linker(linker, data)?;

        let join_handle = Collector::start(collector_rx, self.clone());
        Ok(TraceContext {
            collector,
            join_handle: std::sync::Arc::new(std::sync::Mutex::new(Some(join_handle))),
            shutdown: self.shutdown.clone(),
        })
    }

    pub fn try_send(&self, event: TraceEvent) -> Result<()> {
        self.adapter_tx.send(event)?;
        Ok(())
    }
}

/// The different types of metadata we can send across to a Collector
#[derive(Clone, Debug)]
pub enum AdapterMetadata {
    Datadog(DatadogMetadata),
}
