package scheduler

import (
	schedulerv1 "agentenv/services/api/proto"
	"context"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"testing"
	"time"
)

type testControlPlugin struct{ count int }

func (p *testControlPlugin) Schedule(ctx context.Context, q *schedulerv1.ScheduleRequest, nodes []RichNode, choose func([]RichNode) (RichNode, error)) (*schedulerv1.ScheduleResponse, error) {
	p.count = len(nodes)
	n, err := choose(nodes)
	if err != nil {
		return nil, err
	}
	return &schedulerv1.ScheduleResponse{Node: n.ToProto(), StorageDispatch: []byte("signed")}, nil
}
func (*testControlPlugin) Invoke(context.Context, []byte) ([]byte, error) { return nil, nil }
func TestStorageAdmissionKeepsNativeDiscoveryAndStrategy(t *testing.T) {
	registry := NewAtomicNodeRegistry([]Node{{ID: "ready", Endpoint: "http://ready"}, {ID: "unobserved", Endpoint: "http://unknown"}}, time.Minute)
	_, _, err := registry.Heartbeat(&schedulerv1.HeartbeatRequest{NodeId: "ready", ServiceInstanceId: "boot", Snapshot: &schedulerv1.NodeSnapshot{Status: schedulerv1.NodeStatus_NODE_STATUS_READY}, SandboxIds: []string{"native-id"}}, time.Now())
	if err != nil {
		t.Fatal(err)
	}
	plugin := &testControlPlugin{}
	s := NewService(nil, registry, &RoundRobinStrategy{}, nil, WithControlPlugin(plugin))
	result, err := s.Schedule(context.Background(), &schedulerv1.ScheduleRequest{StorageIntent: []byte("signed")})
	if err != nil {
		t.Fatal(err)
	}
	if plugin.count != 1 || result.Node.NodeId != "ready" {
		t.Fatal("plugin bypassed native discovery/health")
	}
	observed, ok := registry.GetObserved("ready", "", time.Now())
	if !ok || len(observed.SandboxIds) != 1 || observed.SandboxIds[0] != "native-id" {
		t.Fatal("heartbeat instances lost")
	}
	disabled := NewService(nil, registry, &RoundRobinStrategy{}, nil)
	if _, err = disabled.Schedule(context.Background(), &schedulerv1.ScheduleRequest{StorageIntent: []byte("signed")}); status.Code(err) != codes.FailedPrecondition {
		t.Fatal("storage silently bypassed", err)
	}
}
