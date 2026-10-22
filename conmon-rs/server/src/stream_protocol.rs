//! What the streaming protocols of Kubernetes share between exec, attach and
//! port forwarding, whatever the transport.

use std::time::Duration;

/// The header which tells what a stream of a Kubernetes client is for.
pub const STREAM_TYPE_HEADER: &str = "streamType";

/// How long a Kubernetes client may take to open the streams which belong
/// together, the default of the kubelet and CRI-O.
pub const STREAM_CREATION_TIMEOUT: Duration = Duration::from_secs(30);
