@0xffaaf7385bc4adad;

interface Conmon {
    struct VersionResponse {
        version @0 :Text;
    }

    version @0 () -> (response: VersionResponse);

    struct CreateContainerRequest {
        id @0 :Text;
        bundlePath @1 :Text;
        terminal @2 :Bool;
    }

    struct CreateContainerResponse {
        containerPid @0 :UInt32;
    }

    createContainer @1 (request: CreateContainerRequest) -> (response: CreateContainerResponse);
}
