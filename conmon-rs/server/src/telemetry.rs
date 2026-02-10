/// Open Telemetry related source code.
use crate::capnp_util;
use anyhow::{Context, Result};
use capnp::struct_list::Reader;
use clap::crate_name;
use conmon_common::conmon_capnp::conmon;
use nix::unistd::gethostname;
use opentelemetry::{KeyValue, global, propagation::Extractor};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{Resource, propagation::TraceContextPropagator, trace::SdkTracerProvider};
use opentelemetry_semantic_conventions::resource::{HOST_NAME, PROCESS_PID};
use std::{collections::HashMap, process};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// The main structure of this module.
pub struct Telemetry;

impl Telemetry {
    /// Return the telemetry layer if tracing is enabled.
    pub fn layer(endpoint: &str) -> Result<SdkTracerProvider> {
        global::set_text_map_propagator(TraceContextPropagator::new());

        let exporter = SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("build exporter")?;

        let hostname = gethostname()
            .context("get hostname")?
            .to_str()
            .context("convert hostname to string")?
            .to_string();

        let tracer = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                Resource::builder()
                    .with_service_name(crate_name!())
                    .with_attributes([
                        KeyValue::new(PROCESS_PID, process::id() as i64),
                        KeyValue::new(HOST_NAME, hostname),
                    ])
                    .build(),
            )
            .build();

        Ok(tracer)
    }

    /// Set a new parent context from the provided slice data.
    pub fn set_parent_context(reader: Reader<conmon::text_text_map_entry::Owned>) -> Result<()> {
        if reader.is_empty() {
            // Make it a noop if no data is provided.
            return Ok(());
        }

        let metadata = Metadata(capnp_util::into_map(reader)?);
        let ctx = global::get_text_map_propagator(|prop| prop.extract(&metadata));
        let _ = Span::current().set_parent(ctx);

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
