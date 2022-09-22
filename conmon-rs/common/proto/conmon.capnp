@0xffaaf7385bc4adad;

interface Conmon {
    ###############################################
    # Version
    struct VersionRequest {
        verbose @0 :Bool;
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
        }
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
    }

    struct AttachResponse {
    }

    attachContainer @3 (request: AttachRequest) -> (response: AttachResponse);

    ###############################################
    # ReopenLog
    struct ReopenLogRequest {
        id @0 :Text;
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
    }

    struct SetWindowSizeResponse {
    }

    setWindowSizeContainer @5 (request: SetWindowSizeRequest) -> (response: SetWindowSizeResponse);
}
