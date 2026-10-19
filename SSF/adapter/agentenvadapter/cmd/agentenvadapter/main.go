package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	adapter "ssf.dev/ssf/adapter/agentenvadapter"
	"ssf.dev/ssf/auth"
	"syscall"
	"time"
)

func main() {
	if e := run(); e != nil {
		fmt.Fprintln(os.Stderr, e)
		os.Exit(1)
	}
}
func run() error {
	if len(os.Args) < 2 {
		return fmt.Errorf("usage: agentenvadapter control|node|ctl -config FILE")
	}
	role := os.Args[1]
	fs := flag.NewFlagSet(role, flag.ContinueOnError)
	path := fs.String("config", "/etc/agentenvadapter/config.json", "configuration")
	action := fs.String("action", "inspect", "ctl: migrate, inspect, create, delete")
	url := fs.String("url", "", "adapter HTTPS base URL")
	id := fs.String("id", "", "operation ID")
	file := fs.String("request", "", "create JSON file")
	if e := fs.Parse(os.Args[2:]); e != nil {
		return e
	}
	c, e := adapter.Load(*path)
	if e != nil {
		return e
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if role != "ctl" {
		return adapter.Run(ctx, role, c)
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	if *action == "migrate" {
		return adapter.Migrate(ctx, c)
	}
	t, e := auth.TLS(c.Cert, c.Key, c.CA, false)
	if e != nil {
		return e
	}
	client := &adapter.HTTPClient{Client: &http.Client{Transport: &http.Transport{TLSClientConfig: t}, CheckRedirect: func(*http.Request, []*http.Request) error { return fmt.Errorf("redirect forbidden") }}}
	var out json.RawMessage
	method, target := "GET", *url+"/v1/operations/"+*id
	var body any
	switch *action {
	case "inspect":
	case "delete":
		method = "DELETE"
	case "create":
		b, e := os.ReadFile(*file)
		if e != nil {
			return e
		}
		body = json.RawMessage(b)
		method = "POST"
		target = *url + "/v1/sandboxes"
	default:
		return fmt.Errorf("unknown ctl action")
	}
	if e = client.Call(ctx, method, target, body, &out); e != nil {
		return e
	}
	return json.NewEncoder(os.Stdout).Encode(out)
}
