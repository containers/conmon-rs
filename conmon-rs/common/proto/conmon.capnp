@0xffaaf7385bc4adad;

interface Conmon {
    ###############################################
    # Version
    struct VersionRequest {
        verbose @0 :Bool;
        metadataOld @1 :Data; # deprecated
        metadata @2 :Metadata; # Standard metadata to carry.
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
        metadata @9 :Data; # deprecated (never used, remove in next major release)
    }

    version @0 (request: VersionRequest) -> (response: VersionResponse);

    ###############################################
    # CreateContainer
    struct CreateContainerRequest {
        id @0 :Text;
        bundlePath @1 :Text;
        terminal @2 :Bool;
        stdin @3 :Bool;
        exitPaths @4 :List(Text);
        oomExitPaths @5 :List(Text);
        logDrivers @6 :List(LogDriver);
        cleanupCmd @7 :List(Text);
        globalArgs @8 :List(Text);
        commandArgs @9 :List(Text);
        metadataOld @10 :Data; # deprecated
        metadata @11 :Metadata; # Standard metadata to carry.
        envVars @12 :TextTextMap;
        cgroupManager @13 :CgroupManager;
        additionalFds @14 :List(UInt64);
        leakFds @15 :List(UInt64);
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
        timeoutSec @1 :UInt64;
        command @2 :List(Text);
        terminal @3 :Bool;
        metadataOld @4 :Data; # deprecated
        metadata @5 :Metadata; # Standard metadata to carry.
        envVars @6 :TextTextMap;
        cgroupManager @7 :CgroupManager;
    }

    struct ExecSyncContainerResponse {
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
        socketPath @1 :Text;
        execSessionId @2 :Text;
        stopAfterStdinEof @3 :Bool;
        metadataOld @4 :Data; # deprecated
        metadata @5 :Metadata; # Standard metadata to carry.
    }

    struct AttachResponse {
    }

    attachContainer @3 (request: AttachRequest) -> (response: AttachResponse);

    ###############################################
    # ReopenLog
    struct ReopenLogRequest {
        id @0 :Text;
        metadataOld @1 :Data; # deprecated
        metadata @2 :Metadata; # Standard metadata to carry.
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
        metadataOld @3 :Data; # deprecated
        metadata @4 :Metadata; # Standard metadata to carry.

    }

    struct SetWindowSizeResponse {
    }

    setWindowSizeContainer @5 (request: SetWindowSizeRequest) -> (response: SetWindowSizeResponse);

    ###############################################
    # CreateNamespaces
    struct CreateNamespacesRequest {
        metadataOld @0 :Data; # deprecated
        namespaces @1 :List(Namespace); # The list of namespaces to unshare.
        uidMappings @2 :List(Text); # User ID mappings when unsharing the user namespace.
        gidMappings @3 :List(Text); # Group ID mappings when unsharing the user namespace.
        basePath @4 :Text; # The root path for storing the namespaces.
        podId @5 :Text; # The pod identifier.
        metadata @6 :Metadata; # Standard metadata to carry.
    }

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

    # Available namespaces.
    struct NamespaceResponse {
        type @0 :Namespace; # The type of the namespace.
        path @1 :Text; # Path to the directory for the unshared namespaces.
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

    using Metadata = TextTextMap;
    using TextTextMap = List(TextTextMapEntry);
    struct TextTextMapEntry {
        key @0 :Text;
        value @1 :Text;
    }
}
