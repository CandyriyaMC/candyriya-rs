//! OTLP/gRPC (tonic) span pipeline, built only with the `otlp` feature.
//!
//! This path is deliberately minimal and is OFF by default. It has not been
//! exercised on every build environment (local Windows builds may lack a
//! working native/protoc chain); CI covers `--all-features`. A failure to
//! build the exporter or the provider surfaces as
//! [`crate::TelemetryError::Otlp`], never as a panic.
//!
//! No network access happens at install time: the exporter connects lazily
//! on the first batch of spans.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

use crate::config::OtlpExportConfig;
use crate::error::TelemetryError;

/// Build the OTLP span exporter pipeline and the matching tracing layer.
///
/// `S` is the subscriber the returned layer will be stacked onto; it is
/// inferred at the `.with(layer)` call site in the installer.
pub(crate) fn build<S>(
    config: &OtlpExportConfig,
) -> Result<
    (
        tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>,
        SdkTracerProvider,
    ),
    TelemetryError,
>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let endpoint = config.endpoint.trim();
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|err| {
            TelemetryError::Otlp(format!("failed to build the OTLP span exporter: {err}"))
        })?;

    let resource = Resource::builder()
        .with_service_name(config.service_name.trim().to_string())
        .build();
    // Parent-based ratio sampling: respect a sampled parent, otherwise
    // sample `sample_ratio` of root traces.
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(config.sample_ratio)));

    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_sampler(sampler)
        .with_batch_exporter(exporter)
        .build();
    let tracer = provider.tracer("candyriya");

    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    Ok((layer, provider))
}
