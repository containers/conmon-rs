@0xffaaf7385bc4adad;

# The RPC interface of the conmon-rs server, see docs/protocols.md. A call
# which fails carries a message of the server. These prefixes of it stay
# stable, so that clients can tell the failures apart, other messages may
# change:
#
# - "container not found": the server does not know the container of the
#   request, since v1.2.0. Older servers start the message with "child not
#   available".
# - "maximum number of in-flight requests exceeded": too many URLs of the
#   streaming server wait for their client, since v1.2.0, see
#   ServeExecContainer.
#
# The schema only grows, see CONTRIBUTING.md for the rules.
interface Conmon {
    ###############################################
    # Version
    struct VersionRequest {
        # Also report the `cargoTree`.
        verbose @0 :Bool;
        # Retired in v1.0.0, a `Data` field before. Its change to `Void` moved
        # the pointer slots of the fields after it, which broke the wire
        # format, see the compatibility section of docs/cri-o.md.
        metadataOld @1 :Void;
        metadata @2 :Metadata;
    }

    struct VersionResponse {
        # The PID of the server process.
        processId @0 :UInt32;
        # The version of the crate.
        version @1 :Text;
        # The tag of the build, `BUILD_TAG` or else what
        # `git describe --tags --always --dirty` tells, empty without either,
        # like for the static binaries and the Copr RPMs, whose builds pass no
        # tag.
        tag @2 :Text;
        # The git commit of the build.
        commit @3 :Text;
        # The date of the build.
        buildDate @4 :Text;
        # The target triple of the build.
        target @5 :Text;
        # The Rust version of the build.
        rustVersion @6 :Text;
        # The Cargo version of the build.
        cargoVersion @7 :Text;
        # The dependency tree of the build, empty without `verbose` or for a
        # build with `CONMONRS_SKIP_CARGO_TREE`.
        cargoTree @8 :Text;
        # Retired in v1.0.0, the `Data` field `metadata` before, which no
        # server set. Its change to `Void` gave its pointer slot to
        # `features`, which a client since v1.1.0 reads as empty from an
        # older server.
        metadataOld @9 :Void;
        # The optional features this server supports, which the version
        # alone does not tell. Known values: `portforward`,
        # `servePortForwardContainer`; `idletimeout`, the `idleTimeoutSec` of
        # the exec and attach serve requests, which older servers ignore;
        # `streamingprotocols`, exec and attach sessions over SPDY and the
        # WebSocket protocols before `v5.channel.k8s.io`. Servers before
        # unreleased also reported `sdnotify`, the relay of sd_notify
        # messages requested by `sdnotifySocketOld`, and `journaldfields`,
        # the `containerName`, `tagOld` and `labelsOld` of the journald
        # logger, which got retired.
        features @10 :List(Text);
    }

    # Report the version and the features of the server, and its PID.
    version @0 (request: VersionRequest) -> (response: VersionResponse);

    ###############################################
    # CreateContainer
    struct CreateContainerRequest {
        # The ID the runtime gets as an argument, since v1.2.1 an empty one and
        # one which starts with "-" get rejected.
        id @0 :Text;
        # The OCI bundle of the container, an absolute path like all paths
        # of the request.
        bundlePath @1 :Text;
        # Whether the container gets a terminal, which the server sets up
        # through the `--console-socket` of the runtime.
        terminal @2 :Bool;
        # Whether the container gets a stdin, which the attach clients write
        # to.
        stdin @3 :Bool;
        # The files the server writes the exit code of the container to once
        # it exited, as decimal text, 128 plus the signal number if a signal
        # killed it, also for real-time signals since v1.2.2, which gave
        # -1 before. Since unreleased, also a traced process, like one under
        # strace, gets its exit code instead of -1. Absolute paths.
        exitPaths @4 :List(Text);
        # The files the server creates empty if the container got OOM
        # killed, before it writes the exit files. Absolute paths.
        oomExitPaths @5 :List(Text);
        # Where the output of the container goes, every driver gets all of
        # it, see docs/logging.md.
        logDrivers @6 :List(LogDriver);
        # Retired in unreleased, a request which sets it fails as not
        # supported. The command the server started once the container
        # exited before.
        cleanupCmdOld @7 :List(Text);
        # Retired in unreleased, a request which sets it fails as not
        # supported. Arguments of the runtime before the `create` command
        # before. The root of the runtime belongs in the `--runtime-root` of
        # the server, which applies to every call.
        globalArgsOld @8 :List(Text);
        # Arguments of the `create` command of the runtime, before the
        # container ID.
        commandArgs @9 :List(Text);
        # Retired in v1.0.0, a `Data` field before. Its change to `Void` moved
        # the pointer slots of the fields after it, which broke the wire
        # format, see the compatibility section of docs/cri-o.md.
        metadataOld @10 :Void;
        metadata @11 :Metadata;
        # Retired in unreleased, a request which sets it fails as not
        # supported. Environment variables of the runtime on top of the ones
        # of the server before.
        envVarsOld @12 :TextTextMap;
        # The cgroup manager of the runtime, `systemd` adds `--systemd-cgroup`
        # to the runtime arguments.
        cgroupManager @13 :CgroupManager;
        # Retired in unreleased, a request which sets it fails as not
        # supported. Slots of the fd socket of `startFdSocketOld` before,
        # whose file descriptors the runtime got as fd 3 and onwards.
        additionalFdsOld @14 :List(UInt64);
        # Retired in unreleased, a request which sets it fails as not
        # supported. Slots of the fd socket whose file descriptors the server
        # held until the container exited before.
        leakFdsOld @15 :List(UInt64);
        # Retired in unreleased, a request which sets it fails as not
        # supported. The host sd_notify socket which the server relayed the
        # messages of the container to before. Servers since unreleased do
        # not report the `sdnotify` feature, which clients since v1.1.0
        # check first.
        sdnotifySocketOld @16 :Text;
    }

    struct LogDriver {
        # The type of the log driver.
        type @0 :Type;

        # The filesystem path of the log driver, if required, an absolute
        # one.
        path @1 :Text;

        # The maximum log size in bytes, 0 means unlimited.
        maxSize @2 :UInt64;

        # The name of the container, which the journald logger writes as
        # CONTAINER_NAME, like the option of conmon. Since v1.2.2, it is
        # also the SYSLOG_IDENTIFIER, or the short container ID without a
        # name; older servers used `conmonrs`.
        containerName @3 :Text;

        # Retired in unreleased, a request which sets it fails as not
        # supported. The tag of the journal entries before, written as
        # CONTAINER_TAG and SYSLOG_IDENTIFIER.
        tagOld @4 :Text;

        # Retired in unreleased, a request which sets it fails as not
        # supported. Additional fields of every journal entry before.
        labelsOld @5 :TextTextMap;

        enum Type {
            # The CRI logger, requires `path` to be set.
            containerRuntimeInterface @0;
            # Retired in unreleased, the JSON logger before. A request with
            # it fails as not supported.
            jsonOld @1;
            # The journald logger.
            journald @2;
        }
    }

    # The cgroup manager the runtime uses for the container or the exec.
    enum CgroupManager {
        # The runtime gets `--systemd-cgroup`.
        systemd @0;
        # The runtime gets no flag for it and uses the cgroup file system.
        cgroupfs @1;
    }

    struct CreateContainerResponse {
        # The PID of the container process, which the runtime created but did
        # not start yet.
        containerPid @0 :UInt32;
    }

    # Create a container with the runtime and monitor it until it exited:
    # write its output to the log drivers and its exit code to the exit
    # files. The container gets started with the runtime by the caller.
    createContainer @1 (request: CreateContainerRequest) -> (response: CreateContainerResponse);

    ###############################################
    # ExecSync
    struct ExecSyncContainerRequest {
        id @0 :Text; # Rejected like the one of `CreateContainerRequest`.
        # How long the command may run before it gets killed, 0 for no limit.
        # A call which gets dropped does not stop the command.
        timeoutSec @1 :UInt64;
        # The command and its arguments, which the runtime runs in the
        # container.
        command @2 :List(Text);
        # Whether the command gets a terminal.
        terminal @3 :Bool;
        # Retired in v1.0.0, a `Data` field before. Its change to `Void` moved
        # the pointer slots of the fields after it, which broke the wire
        # format, see the compatibility section of docs/cri-o.md.
        metadataOld @4 :Void;
        metadata @5 :Metadata;
        # Retired in unreleased, a request which sets it fails as not
        # supported. Environment variables of the runtime on top of the ones
        # of the server before.
        envVarsOld @6 :TextTextMap;
        # Like the one of `CreateContainerRequest`, `systemd` adds
        # `--systemd-cgroup` to the runtime arguments.
        cgroupManager @7 :CgroupManager;
    }

    struct ExecSyncContainerResponse {
        # The exit code of the command, 128 plus the signal number if a
        # signal killed it, also for real-time signals since v1.2.2, which
        # gave -1 before. Since unreleased, also a traced process, like one
        # under strace, gets its exit code instead of -1. -1 if the server
        # could not tell how it ended, -2 if it could not be started, then
        # `stderr` tells why, and -3 if it timed out. A server without pidfd
        # support also reports -3 if it could not get the exit status, then
        # without `timedOut`.
        exitCode @0 :Int32;
        # The output of the command. Since unreleased, stdout and stderr
        # together hold at most 16 MiB less 64 KiB. Each keeps up to half of
        # it, and more if the other one leaves room, the rest gets dropped,
        # see docs/protocols.md. Before, each took up to 16 MiB.
        stdout @1 :Data;
        stderr @2 :Data;
        timedOut @3 :Bool;
    }

    # Run a command in a container with `exec` of the runtime, wait for it to
    # exit and return its exit code and output.
    execSyncContainer @2 (request: ExecSyncContainerRequest) -> (response: ExecSyncContainerResponse);

    ###############################################
    # Attach
    struct AttachRequest {
        # The ID of the container. The call fails with a message starting
        # with "container not found" for one the server does not know.
        id @0 :Text;
        # The absolute path of the `SOCK_SEQPACKET` socket the server listens
        # on for the attach clients of the container until it exited, see
        # docs/protocols.md. Other paths get rejected since v1.2.2.
        socketPath @1 :Text;
        # Retired in unreleased, the server ignores it. Only logged before,
        # attach sessions are per container.
        execSessionIdOld @2 :Text;
        # Close the container's stdin once the input of a client ended, the
        # output is still forwarded until the container closes it. The stdin
        # of a terminal is never closed. The value of the latest attach call
        # applies to the connections the server accepts afterwards, on any
        # attach socket of the container.
        stopAfterStdinEof @3 :Bool;
        # Retired in v1.0.0, a `Data` field before. Its change to `Void` moved
        # the pointer slots of the fields after it, which broke the wire
        # format, see the compatibility section of docs/cri-o.md.
        metadataOld @4 :Void;
        metadata @5 :Metadata;
    }

    struct AttachResponse {
    }

    # Listen for attach clients of a running container on `socketPath`, see
    # docs/protocols.md. The call returns once the server listens.
    attachContainer @3 (request: AttachRequest) -> (response: AttachResponse);

    ###############################################
    # ReopenLog
    struct ReopenLogRequest {
        # The ID of the container. The call fails with a message starting
        # with "container not found" for one the server does not know.
        id @0 :Text;
        # Retired in v1.0.0, a `Data` field before. Its change to `Void` moved
        # the pointer slots of the fields after it, which broke the wire
        # format, see the compatibility section of docs/cri-o.md.
        metadataOld @1 :Void;
        metadata @2 :Metadata;
    }

    struct ReopenLogResponse {
    }

    # Reopen the log files of a running container, for example after they
    # got rotated.
    reopenLogContainer @4 (request: ReopenLogRequest) -> (response: ReopenLogResponse);

    ###############################################
    # SetWindowSize
    struct SetWindowSizeRequest {
        # The ID of the container. The call fails with a message starting
        # with "container not found" for one the server does not know, and
        # with another one for a container without a terminal.
        id @0 :Text;
        width @1 :UInt16; # columns in characters
        height @2 :UInt16; # rows in characters
        # Retired in v1.0.0, a `Data` field before. Its change to `Void` moved
        # the pointer slots of the fields after it, which broke the wire
        # format, see the compatibility section of docs/cri-o.md.
        metadataOld @3 :Void;
        metadata @4 :Metadata;
    }

    struct SetWindowSizeResponse {
    }

    # Set the size of the terminal of a running container. Since v1.2.2, a
    # size which comes before the runtime connected the terminal gets
    # applied once it does.
    setWindowSizeContainer @5 (request: SetWindowSizeRequest) -> (response: SetWindowSizeResponse);

    ###############################################
    # CreateNamespaces
    struct CreateNamespacesRequest {
        # Retired in v1.0.0, a `Data` field before. Its change to `Void` moved
        # the pointer slots of the fields after it, which broke the wire
        # format, see the compatibility section of docs/cri-o.md.
        metadataOld @0 :Void;
        namespaces @1 :List(Namespace);
        uidMappings @2 :List(Text); # User ID mappings when unsharing the user namespace.
        gidMappings @3 :List(Text); # Group ID mappings when unsharing the user namespace.
        # The root path for storing the namespaces, which has to be absolute.
        basePath @4 :Text;
        # The pod identifier, which names the files of the namespaces below
        # the base path. It must not be empty, "." or "..", consists of the
        # letters A-Z and a-z, the digits 0-9, "_", "." and "-" only, and takes
        # 251 bytes at most.
        podId @5 :Text;
        metadata @6 :Metadata; # Standard metadata to carry.
    }

    # Available namespaces.
    enum Namespace {
        ipc @0; # Unshare the IPC namespace.
        net @1; # Unshare the network namespace.
        pid @2; # Unshare the PID namespace.
        user @3; # Unshare the user namespace.
        uts @4; # Unshare the UTS namespace.
    }

    struct CreateNamespacesResponse {
        namespaces @0 :List(NamespaceResponse);  # The list of created namespaces.
    }

    # A created namespace.
    struct NamespaceResponse {
        type @0 :Namespace; # The type of the namespace.
        path @1 :Text; # Path to the bind mount of the namespace.
    }

    # Retired in unreleased. It created the namespaces of a pod with the
    # pause process of the server before, servers since unreleased fail it
    # with an error which tells that it is not supported. The types above
    # only stay for the layout of the schema.
    createNamespacesOld @6 (request: CreateNamespacesRequest) -> (response: CreateNamespacesResponse);

    ###############################################
    # StartFdSocket
    struct StartFdSocketRequest {
        metadata @0 :Metadata; # Standard metadata to carry.
    }

    struct StartFdSocketResponse {
        path @0 :Text; # The path to the fd socket.
    }

    # Retired in unreleased. It started the fd socket, whose file
    # descriptors `createContainer` referenced by slot, before. Servers since
    # unreleased fail it with an error which tells that it is not supported.
    startFdSocketOld @7 (request: StartFdSocketRequest) -> (response: StartFdSocketResponse);

    ###############################################
    # Helper types

    # The OpenTelemetry trace context of the request, which the client
    # injects with a text map propagator, empty without tracing.
    using Metadata = TextTextMap;
    using TextTextMap = List(TextTextMapEntry);
    struct TextTextMapEntry {
        key @0 :Text;
        value @1 :Text;
    }

    ###############################################
    # ServeExecContainer
    #
    # serveExecContainer, serveAttachContainer and servePortForwardContainer
    # return the URL of a session on the streaming server of the process,
    # which gets started with the first such request. Each URL is good for a
    # single connection within 60 seconds, see docs/protocols.md. While 1000
    # URLs of the kind (exec and attach, port forward) wait for their client,
    # a request fails with an error starting with "maximum number of
    # in-flight requests exceeded".
    struct ServeExecContainerRequest {
        metadata @0 :Metadata; # Standard metadata to carry.
        # The ID of the container. The call fails with a message starting
        # with "container not found" for one the server does not know.
        id @1 :Text;
        # The command and its arguments, which the runtime runs in the
        # container once the client connected.
        command @2 :List(Text);
        # Whether the command gets a terminal.
        tty @3 :Bool;
        # Whether the client sends the stdin of the command.
        stdin @4 :Bool;
        # Whether the client gets the stdout of the command.
        stdout @5 :Bool;
        # Whether the client gets the stderr of the command, a terminal has
        # none.
        stderr @6 :Bool;
        # Like the one of `CreateContainerRequest`, `systemd` adds
        # `--systemd-cgroup` to the runtime arguments.
        cgroupManager @7 :CgroupManager;
        # How long a connection may go without a frame of the client and
        # without data either way before the session fails, 0 disables the
        # timeout. WebSocket pings and pongs, the pings of the server and the
        # answers to them and other control frames of the server do not
        # count. Stdin counts once the server reads it, stdin which backs up
        # behind a process which does not read it stops that. The process
        # gets killed then.
        idleTimeoutSec @8 :UInt64;
    }

    struct ServeExecContainerResponse {
        # The URL of the session, see ServeExecContainer.
        url @0 :Text;
    }

    # Return the URL of an exec session on the streaming server.
    serveExecContainer @8 (request: ServeExecContainerRequest) -> (response: ServeExecContainerResponse);

    ###############################################
    # ServeAttachContainer
    struct ServeAttachContainerRequest {
        metadata @0 :Metadata; # Standard metadata to carry.
        # The ID of the container. The call fails with a message starting
        # with "container not found" for one the server does not know.
        id @1 :Text;
        # Whether the client sends the stdin of the container.
        stdin @2 :Bool;
        # Whether the client gets the stdout of the container.
        stdout @3 :Bool;
        # Whether the client gets the stderr of the container, a terminal has
        # none.
        stderr @4 :Bool;
        # With `stdin`, close the container's stdin once the stdin of the
        # client ended, or the session ended otherwise, the output is still
        # forwarded until the container closes it. A terminal stays open, it
        # gets its end of file character once the stdin of the client ended,
        # like from Ctrl-D.
        stopAfterStdinEof @5 :Bool;
        # Like the one of `ServeExecContainerRequest`, without anything to
        # kill. Stdin which backs up behind a container which does not read
        # it stops counting.
        idleTimeoutSec @6 :UInt64;
    }

    struct ServeAttachContainerResponse {
        # The URL of the session, see ServeExecContainer.
        url @0 :Text;
    }

    # Return the URL of an attach session on the streaming server.
    serveAttachContainer @9 (request: ServeAttachContainerRequest) -> (response: ServeAttachContainerResponse);

    ###############################################
    # ServePortForwardContainer
    struct ServePortForwardContainerRequest {
        metadata @0 :Metadata; # Standard metadata to carry.
        # The absolute path of the network namespace, other ones get rejected
        # since v1.2.2.
        netNsPath @1 :Text;
        # Like the one of `ServeExecContainerRequest`, but per forwarded
        # connection, which gets closed then. A stream which slowly takes its
        # data keeps it active.
        idleTimeoutSec @2 :UInt64;
    }

    struct ServePortForwardContainerResponse {
        # The URL of the session, see ServeExecContainer.
        url @0 :Text;
    }

    # Return the URL of a port forward session on the streaming server.
    servePortForwardContainer @10 (request: ServePortForwardContainerRequest) -> (response: ServePortForwardContainerResponse);
}
