use super::*;
use capnp::{message, struct_list};
use opentelemetry::trace::{TraceContextExt, TraceId, TracerProvider};
use tracing::debug_span;
use tracing_subscriber::layer::SubscriberExt;

const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";

/// The span of a request gets the trace of the client as parent, which has
/// to happen before it got entered.
#[test]
fn spans_continue_the_trace_of_the_client() -> Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let provider = SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

    let mut message = message::Builder::new_default();
    let mut entries: struct_list::Builder<'_, conmon::text_text_map_entry::Owned> =
        message.initn_root(1);
    let mut entry = entries.reborrow().get(0);
    entry.set_key("traceparent");
    entry.set_value(format!("00-{TRACE_ID}-b7ad6b7169203331-01").as_str());
    let metadata = entries.into_reader();

    tracing::subscriber::with_default(subscriber, || {
        let span = debug_span!("request");
        Telemetry::set_parent_context(&span, metadata)?;
        let _enter = span.enter();
        assert_eq!(
            span.context().span().span_context().trace_id(),
            TraceId::from_hex(TRACE_ID)?
        );

        let entered = debug_span!("entered");
        let _enter = entered.enter();
        let err = Telemetry::set_parent_context(&entered, metadata).unwrap_err();
        assert!(
            format!("{err:#}").contains("already been started"),
            "{err:#}"
        );
        Ok(())
    })
}
