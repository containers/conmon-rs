/// Open Telemetry related source code.
use crate::capnp_util;
use anyhow::{Context, Result};
use capnp::struct_list::Reader;
use clap::crate_name;
use conmon_common::conmon_capnp::conmon;
use nix::unistd::gethostname;
use opentelemetry::{global, propagation::Extractor, KeyValue};
use opentelemetry_otlp::{ExportConfig, WithExportConfig};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    runtime::Tokio,
    trace::{self, Tracer},
    Resource,
};
use opentelemetry_semantic_conventions::resource::{HOST_NAME, PROCESS_PID, SERVICE_NAME};
use std::{collections::HashMap, process};
use tracing::{Span, Subscriber};
use tracing_opentelemetry::{OpenTelemetryLayer, OpenTelemetrySpanExt};
use tracing_subscriber::registry::LookupSpan;

/// The main structure of this module.
pub struct Telemetry;

impl Telemetry {
    /// Return the telemetry layer if tracing is enabled.
    pub fn layer<T>(endpoint: &str) -> Result<OpenTelemetryLayer<T, Tracer>>
    where
        T: Subscriber + for<'span> LookupSpan<'span>,
    {
        global::set_text_map_propagator(TraceContextPropagator::new());

        let exporter = opentelemetry_otlp::new_exporter()
            .tonic()
            .with_export_config(ExportConfig {
                endpoint: endpoint.into(),
                ..Default::default()
            });

        let hostname = gethostname()
            .context("get hostname")?
            .to_str()
            .context("convert hostname to string")?
            .to_string();

        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(exporter)
            .with_trace_config(trace::config().with_resource(Resource::new(vec![
                KeyValue::new(SERVICE_NAME, crate_name!()),
                KeyValue::new(PROCESS_PID, process::id() as i64),
                KeyValue::new(HOST_NAME, hostname),
            ])))
            .install_batch(Tokio)
            .context("install tracer")?;

        Ok(tracing_opentelemetry::layer().with_tracer(tracer))
    }

    /// Shutdown the global tracer provider.
    pub fn shutdown() {
        global::shutdown_tracer_provider();
    }

    /// Set a new parent context from the provided slice data.
    pub fn set_parent_context(reader: Reader<conmon::text_text_map_entry::Owned>) -> Result<()> {
        if reader.is_empty() {
            // Make it a noop if no data is provided.
            return Ok(());
        }

        let metadata = Metadata(capnp_util::into_map(reader)?);
        let ctx = global::get_text_map_propagator(|prop| prop.extract(&metadata));
        Span::current().set_parent(ctx);

        Ok(())
    }
}

/// Additional telemetry metadata to carry.
struct Metadata<'a>(HashMap<&'a str, &'a str>);

impl Extractor for Metadata<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).copied()
    }

    /// Collect all the keys from the MetadataMap.
    fn keys(&self) -> Vec<&str> {
        self.0.keys().copied().collect()
    }
}
