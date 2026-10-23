@0xffaaf7385bc4adad;

interface Conmon {
    ###############################################
    # Version
    struct VersionRequest {
        # Also report the `cargoTree`.
        verbose @0 :Bool;
        # Reserved, do not reuse the ordinal.
        metadataOld @1 :Void;
        metadata @2 :Metadata;
    }

    struct VersionResponse {
        processId @0 :UInt32;
        version @1 :Text;
        tag @2 :Text;
        commit @3 :Text;
        buildDate @4 :Text;
        target @5 :Text;
        rustVersion @6 :Text;
        cargoVersion @7 :Text;
        cargoTree @8 :Text;
        # Reserved, do not reuse the ordinal.
        metadataOld @9 :Void;
        # The optional features this server supports, which the version
        # alone does not tell. Known values: `sdnotify`, the relay of
        # sd_notify messages requested by `sdnotifySocket`; `portforward`,
        # `servePortForwardContainer`; `idletimeout`, the `idleTimeoutSec` of
        # the exec and attach serve requests, which older servers ignore;
        # `streamingprotocols`, exec and attach sessions over SPDY and the
        # WebSocket protocols before `v5.channel.k8s.io`.
        features @10 :List(Text);
    }

    version @0 (request: VersionRequest) -> (response: VersionResponse);

    ###############################################
    # CreateContainer
    struct CreateContainerRequest {
        id @0 :Text;
        bundlePath @1 :Text;
        terminal @2 :Bool;
        stdin @3 :Bool;
        # The files the server writes the exit code of the container to once
        # it exited, as decimal text, 128 plus the signal number if a signal
        # killed it.
        exitPaths @4 :List(Text);
        # The files the server creates empty if the container got OOM
        # killed, before it writes the exit files.
        oomExitPaths @5 :List(Text);
        logDrivers @6 :List(LogDriver);
        # The command the server starts once the container exited and the
        # exit files got written, without waiting for it.
        cleanupCmd @7 :List(Text);
        # Arguments of the runtime before the `create` command.
        globalArgs @8 :List(Text);
        # Arguments of the `create` command of the runtime, before the
        # container ID.
        commandArgs @9 :List(Text);
        # Reserved, do not reuse the ordinal.
        metadataOld @10 :Void;
        metadata @11 :Metadata;
        # Environment variables of the runtime, on top of the ones of the
        # server.
        envVars @12 :TextTextMap;
        cgroupManager @13 :CgroupManager;
        # Slots of the fd socket, see `startFdSocket`, whose file descriptors
        # the runtime gets as fd 3 and onwards in their order. With
        # `--preserve-fds` in `commandArgs`, it passes them on.
        additionalFds @14 :List(UInt64);
        # Slots of the fd socket whose file descriptors the server holds
        # until the container exited, without passing them on.
        leakFds @15 :List(UInt64);
        # The host sd_notify socket to relay messages to, usually the
        # `NOTIFY_SOCKET` of systemd. If set, the server binds
        # `<bundlePath>/notify/notify.sock` before it runs the runtime and
        # relays the datagrams sent there to the host socket until the
        # container exits. Like conmon, it only passes the assignments
        # `READY=1`, `RELOADING=1`, `STOPPING=1`, `WATCHDOG=1`,
        # `WATCHDOG=trigger`, `STATUS=`, `ERRNO=`, `BUSERROR=` and
        # `MONOTONIC_USEC=`. The caller mounts the `notify` directory into the
        # container and sets `NOTIFY_SOCKET` for its process, which the
        # runtime does not see.
        sdnotifySocket @16 :Text;
    }

    struct LogDriver {
        # The type of the log driver.
        type @0 :Type;

        # The filesystem path of the log driver, if required.
        path @1 :Text;

        # The maximum log size in bytes, 0 means unlimited.
        maxSize @2 :UInt64;

        enum Type {
            # The CRI logger, requires `path` to be set.
            containerRuntimeInterface @0;
            # The JSON logger, requires `path` to be set.
            json @1;
            # The journald logger.
            journald @2;
        }
    }

    enum CgroupManager {
        systemd @0;
        cgroupfs @1;
    }

    struct CreateContainerResponse {
        containerPid @0 :UInt32;
    }

    createContainer @1 (request: CreateContainerRequest) -> (response: CreateContainerResponse);

    ###############################################
    # ExecSync
    struct ExecSyncContainerRequest {
        id @0 :Text;
        # How long the command may run before it gets killed, 0 for no limit.
        # A call which gets dropped does not stop the command.
        timeoutSec @1 :UInt64;
        command @2 :List(Text);
        terminal @3 :Bool;
        # Reserved, do not reuse the ordinal.
        metadataOld @4 :Void;
        metadata @5 :Metadata;
        envVars @6 :TextTextMap;
        cgroupManager @7 :CgroupManager;
    }

    struct ExecSyncContainerResponse {
        # The exit code of the command, 128 plus the signal number if a
        # signal killed it. -1 if the server could not tell how it ended, -2
        # if it could not be started, then `stderr` tells why, and -3 if it
        # timed out. A server without pidfd support also reports -3 if it
        # could not get the exit status, then without `timedOut`.
        exitCode @0 :Int32;
        stdout @1 :Data;
        stderr @2 :Data;
        timedOut @3 :Bool;
    }

    execSyncContainer @2 (request: ExecSyncContainerRequest) -> (response: ExecSyncContainerResponse);

    ###############################################
    # Attach
    struct AttachRequest {
        id @0 :Text;
        # The path of the `SOCK_SEQPACKET` socket the server listens on for
        # the attach clients of the container until it exited, see usage.md.
        socketPath @1 :Text;
        # Deprecated, only logged, attach sessions are per container.
        execSessionId @2 :Text;
        stopAfterStdinEof @3 :Bool;
        # Reserved, do not reuse the ordinal.
        metadataOld @4 :Void;
        metadata @5 :Metadata;
    }

    struct AttachResponse {
    }

    attachContainer @3 (request: AttachRequest) -> (response: AttachResponse);

    ###############################################
    # ReopenLog
    struct ReopenLogRequest {
        id @0 :Text;
        # Reserved, do not reuse the ordinal.
        metadataOld @1 :Void;
        metadata @2 :Metadata;
    }

    struct ReopenLogResponse {
    }

    reopenLogContainer @4 (request: ReopenLogRequest) -> (response: ReopenLogResponse);

    ###############################################
    # SetWindowSize
    struct SetWindowSizeRequest {
        id @0 :Text; # container identifier
        width @1 :UInt16; # columns in characters
        height @2 :UInt16; # rows in characters
        # Reserved, do not reuse the ordinal.
        metadataOld @3 :Void;
        metadata @4 :Metadata;
    }

    struct SetWindowSizeResponse {
    }

    setWindowSizeContainer @5 (request: SetWindowSizeRequest) -> (response: SetWindowSizeResponse);

    ###############################################
    # CreateNamespaces
    struct CreateNamespacesRequest {
        # Reserved, do not reuse the ordinal.
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

    createNamespaces @6 (request: CreateNamespacesRequest) -> (response: CreateNamespacesResponse);

    ###############################################
    # StartFdSocket
    struct StartFdSocketRequest {
        metadata @0 :Metadata; # Standard metadata to carry.
    }

    struct StartFdSocketResponse {
        path @0 :Text; # The path to the fd socket.
    }

    startFdSocket @7 (request: StartFdSocketRequest) -> (response: StartFdSocketResponse);

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
    # single connection within 60 seconds, see usage.md. While 1000 URLs of
    # the kind (exec and attach, port forward) wait for their client, a
    # request fails with an error starting with "maximum number of in-flight
    # requests exceeded".
    struct ServeExecContainerRequest {
        metadata @0 :Metadata; # Standard metadata to carry.
        id @1 :Text;
        command @2 :List(Text);
        tty @3 :Bool;
        stdin @4 :Bool;
        stdout @5 :Bool;
        stderr @6 :Bool;
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

    serveExecContainer @8 (request: ServeExecContainerRequest) -> (response: ServeExecContainerResponse);

    ###############################################
    # ServeAttachContainer
    struct ServeAttachContainerRequest {
        metadata @0 :Metadata; # Standard metadata to carry.
        id @1 :Text;
        stdin @2 :Bool;
        stdout @3 :Bool;
        stderr @4 :Bool;
        stopAfterStdinEof @5 :Bool;
        # How long a connection may go without a frame of the client and
        # without data either way before the session fails, 0 disables the
        # timeout. WebSocket pings and pongs, the pings of the server and the
        # answers to them and other control frames of the server do not
        # count. Stdin counts once the server reads it, stdin which backs up
        # behind a container which does not read it stops that.
        idleTimeoutSec @6 :UInt64;
    }

    struct ServeAttachContainerResponse {
        # The URL of the session, see ServeExecContainer.
        url @0 :Text;
    }

    serveAttachContainer @9 (request: ServeAttachContainerRequest) -> (response: ServeAttachContainerResponse);

    ###############################################
    # ServePortForwardContainer
    struct ServePortForwardContainerRequest {
        metadata @0 :Metadata; # Standard metadata to carry.
        netNsPath @1 :Text;
        # How long a connection may go without a frame of the client and
        # without data either way before it gets closed, 0 disables the
        # timeout. WebSocket pings and pongs, the pings of the server and the
        # answers to them and other control frames of the server do not
        # count.
        idleTimeoutSec @2 :UInt64;
    }

    struct ServePortForwardContainerResponse {
        # The URL of the session, see ServeExecContainer.
        url @0 :Text;
    }

    servePortForwardContainer @10 (request: ServePortForwardContainerRequest) -> (response: ServePortForwardContainerResponse);
}
