@0xffaaf7385bc4adad;

interface Conmon {
    struct VersionResponse {
        version @0 :Text;
        tag @1: Text;
        commit @2: Text;
        buildDate @3: Text;
        rustVersion @4: Text;
    }

    version @0 () -> (response: VersionResponse);

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
}
