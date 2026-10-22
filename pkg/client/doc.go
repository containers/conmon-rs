// Package client is the Go client of the conmon-rs server, a container
// monitor which runs once per pod.
//
// # Server lifecycle
//
// New starts a server for the ServerRunDir of the configuration, or connects
// to the one which already runs there, so that a restarted caller finds its
// containers again. All clients of one ServerRunDir share its server. The
// settings of the server process, like Runtime, LogLevel or Env, only apply
// to a server New starts. The server runs in the background, it outlives the
// client and its process.
//
// A client which is not needed anymore has to be closed, otherwise its
// connection and goroutines leak. Close only releases the client, the server
// and its containers keep running. Shutdown stops the server, which kills
// its containers before it exits. If it does not exit in time, Shutdown
// fails with ErrShutdownTimeout and KillContext ends it with SIGKILL.
//
// # Features
//
// Servers of older releases do not support every method or field. The
// Features of the response of Version list what the server supports beyond
// what its version tells, see the Feature constants. Methods which need a
// feature fail with ErrUnsupported on a server without it, fields which an
// older server ignores say so in their documentation.
//
// # Errors
//
// The server reports its failures as messages. A request for a container
// the server does not know fails with an error matching
// ErrContainerNotFound, use errors.Is to check for it. ExecSyncContainer
// leaves the container to the runtime instead, the result of one which the
// runtime does not know has ExecExitCodeNotStarted.
//
// # Streaming
//
// ServeExecContainer, ServeAttachContainer and ServePortForwardContainer
// return a URL on the streaming server of the server process, which a client
// like the kubelet connects to with the Kubernetes streaming protocols. The
// streaming server listens on 127.0.0.1 and gets started with the first such
// request. Each URL carries a token which is good for a single connection
// within 60 seconds, the server keeps a bounded number of unused ones.
// AttachContainer attaches to a container through a local socket instead.
package client
