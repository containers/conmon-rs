package main

import (
	"context"
	"fmt"
	"net"

	"capnproto.org/go/capnp/v3/rpc"

	"github.com/containers/conmon-rs/internal/pkg/proto"
)

func main() {
	if err := run(); err != nil {
		panic(err)
	}
}

func run() error {
	const socketAddr = "conmon.sock"

	socketConn, err := net.Dial("unix", socketAddr)
	if err != nil {
		return err
	}

	conn := rpc.NewConn(rpc.NewStreamTransport(socketConn), nil)
	defer conn.Close()

	ctx := context.Background()
	client := proto.Conmon{Client: conn.Bootstrap(ctx)}

	future, free := client.Version(ctx, nil)
	defer free()

	result, err := future.Struct()
	if err != nil {
		return err
	}

	response, err := result.Response()
	if err != nil {
		return err
	}

	version, err := response.Version()
	if err != nil {
		return err
	}

	fmt.Printf("Version response: %s\n", version)
	return nil
}
