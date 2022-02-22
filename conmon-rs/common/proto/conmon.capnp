@0xffaaf7385bc4adad;

interface Conmon {
    ###############################################
    # Version
    struct VersionResponse {
        version @0 :Text;
        tag @1: Text;
        commit @2: Text;
        buildDate @3: Text;
        rustVersion @4: Text;
    }

    version @0 () -> (response: VersionResponse);

    ###############################################
    # CreateContainer
    struct CreateContainerRequest {
        id @0 :Text;
        bundlePath @1 :Text;
        terminal @2 :Bool;
        exitPaths @3 :List(Text);
    }

    struct CreateContainerResponse {
        containerPid @0 :UInt32;
    }

    createContainer @1 (request: CreateContainerRequest) -> (response: CreateContainerResponse);

    ###############################################
    # ExecSync
    struct ExecSyncContainerRequest {
        id @0 :Text;
        timeout  @1 :Int32;
        command @2 :List(Text);
    }

    struct ExecSyncContainerResponse {
        exitCode @0 :Int32;
        stdout @1 :Text;
        stderr @2 :Text;
    }

    execSyncContainer @2 (request: ExecSyncContainerRequest) -> (response: ExecSyncContainerResponse);
}