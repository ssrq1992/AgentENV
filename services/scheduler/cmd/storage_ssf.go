//go:build ssf

package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"os"
	"time"

	schedulerv1 "agentenv/services/api/proto"
	scheduler "agentenv/services/scheduler/internal"
	"agentenv/services/shared/storage"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	adapter "ssf.dev/ssf/adapter/agentenvadapter"
)

type nativeSSFPlugin struct {
	control                          *adapter.NativeControl
	registry                         *scheduler.AtomicNodeRegistry
	gatewayKey, dispatchKey, nodeKey []byte
}

func initializeStorage(ctx context.Context, registry *scheduler.AtomicNodeRegistry) (scheduler.ControlPlugin, func(), error) {
	path := os.Getenv("AENV_SSF_CONFIG")
	if path == "" {
		return nil, func() {}, nil
	}
	cfg, err := adapter.LoadNativeConfig(path)
	if err != nil {
		return nil, nil, err
	}
	gatewayKey, err := storage.LoadKey(cfg.GatewayKeyFile)
	if err != nil {
		return nil, nil, err
	}
	dispatchKey, err := storage.LoadKey(cfg.DispatchKeyFile)
	if err != nil {
		return nil, nil, err
	}
	nodeKey, err := storage.LoadKey(cfg.NodeKeyFile)
	if err != nil {
		return nil, nil, err
	}
	if string(gatewayKey) == string(nodeKey) || string(dispatchKey) == string(nodeKey) || string(dispatchKey) == string(gatewayKey) {
		return nil, nil, errors.New("gateway, node and scheduler storage keys must be distinct")
	}
	control, closeHost, err := adapter.OpenNativeControl(ctx, cfg)
	if err != nil {
		return nil, nil, err
	}
	refresh := control.Refresh
	control.Refresh = func(ctx context.Context, candidate adapter.NativeCandidate) (adapter.NativeCandidate, error) {
		observed, ok := registry.GetObserved(candidate.ID, "", time.Now())
		if !ok || observed.GetSnapshot().GetStatus() != schedulerv1.NodeStatus_NODE_STATUS_READY {
			return candidate, errors.New("native node heartbeat is no longer ready")
		}
		snapshot := observed.GetSnapshot()
		candidate.Endpoint = observed.GetEndpoint()
		candidate.Capacity = adapter.Resources{CPU: int64(snapshot.GetCpuCount()), MemoryMiB: int64(snapshot.GetMemoryTotalBytes() / (1 << 20))}
		candidate.Allocated = adapter.Resources{CPU: int64(snapshot.GetAllocatedCpu()), MemoryMiB: int64((snapshot.GetAllocatedMemoryBytes() + (1 << 20) - 1) / (1 << 20))}
		candidate.SandboxIDs = observed.GetSandboxIds()
		return refresh(ctx, candidate)
	}
	recoveryContext, cancelRecovery := context.WithCancel(ctx)
	recoveryDone := make(chan struct{})
	go func() {
		defer close(recoveryDone)
		control.RecoverNative(recoveryContext, time.Duration(cfg.RecoverySeconds)*time.Second)
	}()
	shutdown := func() { cancelRecovery(); <-recoveryDone; closeHost() }
	return &nativeSSFPlugin{control: control, registry: registry, gatewayKey: gatewayKey, dispatchKey: dispatchKey, nodeKey: nodeKey}, shutdown, nil
}
func (p *nativeSSFPlugin) Schedule(ctx context.Context, q *schedulerv1.ScheduleRequest, nodes []scheduler.RichNode, choose func([]scheduler.RichNode) (scheduler.RichNode, error)) (*schedulerv1.ScheduleResponse, error) {
	if len(q.GetStorageIntent()) == 0 {
		if q.GetHint() != nil {
			return nil, status.Error(codes.Unauthenticated, "storage-enabled launch requires signed native intent")
		}
		n, err := choose(nodes)
		if err != nil {
			return nil, err
		}
		return &schedulerv1.ScheduleResponse{Node: n.ToProto()}, nil
	}
	envelope, err := storage.Verify(p.gatewayKey, q.StorageIntent, "gateway", time.Now())
	if err != nil {
		return nil, status.Error(codes.Unauthenticated, err.Error())
	}
	var intent storage.LaunchIntent
	if err = json.Unmarshal(envelope.Payload, &intent); err != nil {
		return nil, status.Error(codes.InvalidArgument, err.Error())
	}
	if err = intent.Validate(); err != nil {
		return nil, status.Error(codes.InvalidArgument, err.Error())
	}
	if intent.NodeID != "" || intent.OwnerID != "" {
		return nil, status.Error(codes.PermissionDenied, "gateway cannot impersonate a pinned native lifecycle")
	}
	candidates := make([]adapter.NativeCandidate, 0, len(nodes))
	byID := map[string]scheduler.RichNode{}
	for _, n := range nodes {
		byID[n.ID] = n
		candidates = append(candidates, adapter.NativeCandidate{ID: n.ID, Endpoint: n.Endpoint, Capacity: adapter.Resources{CPU: int64(n.Snapshot.GetCpuCount()), MemoryMiB: int64(n.Snapshot.GetMemoryTotalBytes() / (1 << 20))}, Allocated: adapter.Resources{CPU: int64(n.Snapshot.GetAllocatedCpu()), MemoryMiB: int64((n.Snapshot.GetAllocatedMemoryBytes() + (1 << 20) - 1) / (1 << 20))}, SandboxIDs: n.SandboxIDs})
	}
	selection := func(cs []adapter.NativeCandidate) (adapter.NativeCandidate, error) {
		rich := make([]scheduler.RichNode, 0, len(cs))
		for _, c := range cs {
			rich = append(rich, byID[c.ID])
		}
		selected, err := choose(rich)
		if err != nil {
			return adapter.NativeCandidate{}, err
		}
		for _, c := range cs {
			if c.ID == selected.ID {
				return c, nil
			}
		}
		return adapter.NativeCandidate{}, errors.New("native strategy selected unknown candidate")
	}
	d, err := p.control.Prepare(ctx, adapter.NativeIntent{RequestID: envelope.RequestID, Tenant: p.control.Principal.Tenant, Path: intent.Path, Body: intent.Body, BodyDigest: intent.BodyDigest}, candidates, selection)
	if err != nil {
		return nil, status.Error(codes.FailedPrecondition, err.Error())
	}
	signed, err := storage.Sign(p.dispatchKey, "scheduler", d.RequestID, d, time.Now())
	if err != nil {
		return nil, err
	}
	return &schedulerv1.ScheduleResponse{Node: &schedulerv1.Node{NodeId: d.Candidate.ID, Endpoint: d.Candidate.Endpoint}, StorageDispatch: signed}, nil
}
func (p *nativeSSFPlugin) Invoke(ctx context.Context, raw []byte) ([]byte, error) {
	envelope, err := storage.Verify(p.nodeKey, raw, "node", time.Now())
	if err != nil {
		return nil, status.Error(codes.Unauthenticated, err.Error())
	}
	var command adapter.NativeCommand
	if err = json.Unmarshal(envelope.Payload, &command); err != nil {
		return nil, status.Error(codes.InvalidArgument, err.Error())
	}
	if _, ok := p.registry.Resolve(command.NodeID); !ok {
		// Draining nodes can leave ordinary discovery before the final stop
		// callback. Cleanup still requires the live Kubernetes Pod/node and
		// authenticated plugin identity, independently of scheduling health.
		if (command.Action != "stopped" && command.Action != "delete" && command.Action != "checkpoint.published" && command.Action != "checkpoint.deleted") || p.control.RefreshStopped == nil {
			return nil, status.Error(codes.PermissionDenied, "native node not discovered")
		}
		if _, err := p.control.RefreshStopped(ctx, adapter.NativeCandidate{ID: command.NodeID}); err != nil {
			return nil, status.Error(codes.PermissionDenied, "native cleanup node identity unavailable")
		}
	}
	var candidates []adapter.NativeCandidate
	if command.Action == "resume" || command.Action == "fork" || command.Action == "compute" {
		observed, ok := p.registry.GetObserved(command.NodeID, "", time.Now())
		if !ok || observed.GetSnapshot().GetStatus() != schedulerv1.NodeStatus_NODE_STATUS_READY {
			return nil, status.Error(codes.Unavailable, "native execution node is not ready")
		}
		snap := observed.GetSnapshot()
		candidates = []adapter.NativeCandidate{{ID: command.NodeID, Endpoint: observed.GetEndpoint(), Capacity: adapter.Resources{CPU: int64(snap.GetCpuCount()), MemoryMiB: int64(snap.GetMemoryTotalBytes() / (1 << 20))}, Allocated: adapter.Resources{CPU: int64(snap.GetAllocatedCpu()), MemoryMiB: int64((snap.GetAllocatedMemoryBytes() + (1 << 20) - 1) / (1 << 20))}, SandboxIDs: observed.GetSandboxIds()}}
	}
	choose := func(cs []adapter.NativeCandidate) (adapter.NativeCandidate, error) {
		if len(cs) != 1 {
			return adapter.NativeCandidate{}, errors.New("native child requires original placement")
		}
		return cs[0], nil
	}
	reply, err := p.control.Command(ctx, command, candidates, choose)
	var result any = reply.Data
	if err == nil && reply.Dispatch != nil {
		var signed []byte
		signed, err = storage.Sign(p.dispatchKey, "scheduler", reply.Dispatch.RequestID, reply.Dispatch, time.Now())
		result = map[string]any{"context": base64.RawURLEncoding.EncodeToString(signed), "body_digest": reply.Dispatch.BodyDigest}
	}

	if err != nil {
		return nil, status.Error(codes.FailedPrecondition, err.Error())
	}
	return storage.Sign(p.dispatchKey, "scheduler", envelope.RequestID, result, time.Now())
}
