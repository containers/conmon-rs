/// Open Telemetry related source code.
use anyhow::{Context, Result};
use clap::crate_name;
use nix::unistd::gethostname;
use opentelemetry::{
    global,
    runtime::Tokio,
    sdk::{
        propagation::TraceContextPropagator,
        trace::{self, Tracer},
        Resource,
    },
};
use opentelemetry_otlp::{ExportConfig, WithExportConfig};
use opentelemetry_semantic_conventions::resource::{HOST_NAME, PROCESS_PID, SERVICE_NAME};
use std::process;
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetryLayer;
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
                SERVICE_NAME.string(crate_name!()),
                PROCESS_PID.i64(process::id() as i64),
                HOST_NAME.string(hostname),
            ])))
            .install_batch(Tokio)
            .context("install tracer")?;

        Ok(tracing_opentelemetry::layer().with_tracer(tracer))
    }

    /// Shutdown the global tracer provider.
    pub fn shutdown() {
        global::shutdown_tracer_provider();
    }
}
