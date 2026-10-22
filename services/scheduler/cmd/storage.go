package main

import (
	schedulerv1 "agentenv/services/api/proto"
	scheduler "agentenv/services/scheduler/internal"
	"context"
	"errors"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"net"
	"net/http"
	"os"
	control "ssf.dev/control"
	"time"
)

type controlTransport interface {
	Call(context.Context, string, control.Request) (control.Response, error)
}
type nativeSSFPlugin struct{ client controlTransport }

func initializeStorage(ctx context.Context, registry *scheduler.AtomicNodeRegistry) (scheduler.ControlPlugin, func(), error) {
	path := os.Getenv("AENV_SSF_CONFIG")
	if path == "" {
		return nil, func() {}, nil
	}
	cfg, e := control.Load(path)
	if e != nil {
		return nil, nil, e
	}
	client, e := control.New(cfg)
	if e != nil {
		return nil, nil, e
	}
	if cfg.ObserveListen == "" {
		client.Close()
		return nil, nil, errors.New("native observation listener required")
	}
	tls, e := control.TLS(cfg, true)
	if e != nil {
		client.Close()
		return nil, nil, e
	}
	listener, e := net.Listen("tcp", cfg.ObserveListen)
	if e != nil {
		client.Close()
		return nil, nil, e
	}
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/"+control.Version+"/ObserveNodes" {
			http.NotFound(w, r)
			return
		}
		if !control.HasPeer(r.TLS, []string{cfg.Peer}) {
			http.Error(w, "control peer denied", 403)
			return
		}
		q, err := control.Decode(w, r)
		if err != nil || q.Runtime != "agentenv" {
			control.Write(w, control.Response{}, errors.New("invalid observation request"))
			return
		}
		out := control.Response{}
		for _, n := range registry.Snapshot(false) {
			v, ok := registry.GetObserved(n.ID, "", time.Now())
			if !ok || v.GetSnapshot().GetStatus() != schedulerv1.NodeStatus_NODE_STATUS_READY {
				continue
			}
			s := v.GetSnapshot()
			out.Candidates = append(out.Candidates, control.Node{ID: n.ID, Endpoint: v.GetEndpoint(), Capacity: control.Resources{CPU: int64(s.GetCpuCount()), MemoryMiB: int64(s.GetMemoryTotalBytes() / (1 << 20))}, Allocated: control.Resources{CPU: int64(s.GetAllocatedCpu()), MemoryMiB: int64((s.GetAllocatedMemoryBytes() + (1 << 20) - 1) / (1 << 20))}, SandboxIDs: v.GetSandboxIds()})
		}
		control.Write(w, out, nil)
	})
	server := &http.Server{Handler: handler, TLSConfig: tls, ReadHeaderTimeout: 5 * time.Second, IdleTimeout: time.Minute}
	go func() { _ = server.ServeTLS(listener, "", "") }()
	close := func() {
		drain, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = server.Shutdown(drain)
		client.Close()
	}
	return &nativeSSFPlugin{client: client}, close, nil
}
func (p *nativeSSFPlugin) Schedule(ctx context.Context, q *schedulerv1.ScheduleRequest, nodes []scheduler.RichNode, choose func([]scheduler.RichNode) (scheduler.RichNode, error)) (*schedulerv1.ScheduleResponse, error) {
	if len(q.GetStorageIntent()) == 0 {
		if q.GetHint() != nil {
			return nil, status.Error(codes.Unauthenticated, "storage-enabled launch requires signed native intent")
		}
		n, e := choose(nodes)
		if e != nil {
			return nil, e
		}
		return &schedulerv1.ScheduleResponse{Node: n.ToProto()}, nil
	}
	req := control.Request{Runtime: "agentenv", Envelope: q.StorageIntent}
	byID := map[string]scheduler.RichNode{}
	for _, n := range nodes {
		byID[n.ID] = n
		req.Candidates = append(req.Candidates, control.Node{ID: n.ID, Endpoint: n.Endpoint})
	}
	resolved, e := p.client.Call(ctx, "ResolveLaunch", req)
	if e != nil {
		return nil, storageFailure(e)
	}
	req.LaunchID = resolved.LaunchID
	if len(resolved.Dispatch) > 0 {
		return dispatchResponse(resolved, byID)
	}
	remaining := []scheduler.RichNode{}
	for _, c := range resolved.Candidates {
		n, ok := byID[c.ID]
		if ok {
			remaining = append(remaining, n)
		}
	}
	for len(remaining) > 0 {
		var selected scheduler.RichNode
		if resolved.Selected != nil {
			var ok bool
			selected, ok = byID[resolved.Selected.ID]
			if !ok {
				return nil, status.Error(codes.Unavailable, "reserved native node not ready")
			}
		} else {
			selected, e = choose(remaining)
			if e != nil {
				return nil, e
			}
		}
		req.Selected = selected.ID
		prepared, err := p.client.Call(ctx, "PrepareLaunch", req)
		if err == nil {
			return dispatchResponse(prepared, byID)
		}
		var refusal *control.Error
		if errors.As(err, &refusal) && refusal.Code == "capacity" && resolved.Selected == nil {
			next := remaining[:0]
			for _, n := range remaining {
				if n.ID != selected.ID {
					next = append(next, n)
				}
			}
			remaining = next
			continue
		}
		// A lost reply never means an unstarted VM. Inspect this launch once; the
		// service either returns its exact dispatch or retains the unknown operation.
		observed, inspectErr := p.client.Call(ctx, "InspectOperation", req)
		if inspectErr == nil && len(observed.Dispatch) > 0 {
			return dispatchResponse(observed, byID)
		}
		return nil, storageFailure(err)
	}
	return nil, status.Error(codes.ResourceExhausted, "no native storage-compatible capacity")
}
func dispatchResponse(r control.Response, nodes map[string]scheduler.RichNode) (*schedulerv1.ScheduleResponse, error) {
	if r.Selected == nil || len(r.Dispatch) == 0 {
		return nil, errors.New("incomplete control dispatch")
	}
	n, ok := nodes[r.Selected.ID]
	if !ok || n.Endpoint != r.Selected.Endpoint {
		return nil, errors.New("dispatch differs from native discovery")
	}
	return &schedulerv1.ScheduleResponse{Node: n.ToProto(), StorageDispatch: r.Dispatch}, nil
}

// Preserve native gRPC/Gateway error categories across the private transport.
func storageFailure(err error) error {
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return status.FromContextError(err).Err()
	}
	var remote *control.Error
	if errors.As(err, &remote) {
		code := codes.FailedPrecondition
		switch remote.Code {
		case "unauthenticated":
			code = codes.Unauthenticated
		case "permission":
			code = codes.PermissionDenied
		case "invalid":
			code = codes.InvalidArgument
		case "capacity":
			code = codes.ResourceExhausted
		}
		return status.Error(code, remote.Message)
	}
	return status.Error(codes.Unavailable, err.Error())
}
