// ssf-test-driver must only run in disposable Linux integration environments.
package main

import (
	"flag"
	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc"
	"log"
	"net"
	"os"
	"path/filepath"
	"ssf.dev/ssf/testing/csidriver"
)

func main() {
	socket := flag.String("socket", "/csi/csi.sock", "UDS")
	root := flag.String("root", "/data/ssf", "fixture data")
	node := flag.String("node", "test-node", "CSI node ID")
	mode := flag.String("mode", "file", "file or block")
	attach := flag.Bool("attach", false, "simulate ControllerPublish")
	flag.Parse()
	if *mode != "file" && *mode != "block" {
		log.Fatal("invalid mode")
	}
	if e := os.MkdirAll(filepath.Dir(*socket), 0700); e != nil {
		log.Fatal(e)
	}
	if _, e := os.Lstat(*socket); e == nil {
		log.Fatal("socket exists; verify old process stopped before removing")
	}
	l, e := net.Listen("unix", *socket)
	if e != nil {
		log.Fatal(e)
	}
	defer os.Remove(*socket)
	s := grpc.NewServer()
	d := &csidriver.Driver{Root: *root, NodeID: *node, Mode: *mode, Attach: *attach}
	csi.RegisterIdentityServer(s, d)
	csi.RegisterControllerServer(s, d)
	csi.RegisterNodeServer(s, d)
	log.Fatal(s.Serve(l))
}
