# Tracing

Scripts to look at the OpenTelemetry traces of a server built with the
`tracing` feature, see [usage.md](../../usage.md#tracing).

[start](start) runs Jaeger in a container with podman or docker, on the host
network, and waits for its OTLP gRPC port 4317, the default
`--tracing-endpoint` of the server. [stop](stop) removes the container again.

```console
> contrib/tracing/start
> cargo build --release --features tracing
> target/release/conmonrs --runtime /usr/bin/crun --runtime-dir /tmp/conmonrs \
    --skip-fork -d stdout --enable-tracing
> contrib/tracing/stop
```

The Jaeger UI listens on <http://localhost:16686>. With the Go client, set
`Tracing: &client.Tracing{Enabled: true}` in the `ConmonServerConfig` to start
the server with tracing. The client passes the span of a call along only with
an OpenTelemetry propagator registered, see
[usage.md](../../usage.md#tracing).
