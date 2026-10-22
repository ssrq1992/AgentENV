package main

import (
	pb "agentenv/services/api/proto"
	scheduler "agentenv/services/scheduler/internal"
	"context"
	"errors"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	control "ssf.dev/control"
	"testing"
)

type fakeControl func(context.Context, string, control.Request) (control.Response, error)

func (f fakeControl) Call(c context.Context, m string, q control.Request) (control.Response, error) {
	return f(c, m, q)
}
func TestRemoteAdmissionUsesOriginalStrategyAndRetriesOnlyKnownCapacity(t *testing.T) {
	nodes := []scheduler.RichNode{{Node: scheduler.Node{ID: "a", Endpoint: "http://a"}}, {Node: scheduler.Node{ID: "b", Endpoint: "http://b"}}}
	selections := 0
	prepares := []string{}
	p := &nativeSSFPlugin{client: fakeControl(func(_ context.Context, m string, q control.Request) (control.Response, error) {
		switch m {
		case "ResolveLaunch":
			return control.Response{LaunchID: "launch", Candidates: []control.Node{{ID: "a"}, {ID: "b"}}}, nil
		case "PrepareLaunch":
			if q.LaunchID != "launch" {
				t.Fatal("launch identity lost")
			}
			prepares = append(prepares, q.Selected)
			if q.Selected == "a" {
				return control.Response{}, &control.Error{Code: "capacity"}
			}
			return control.Response{Selected: &control.Node{ID: "b", Endpoint: "http://b"}, Dispatch: []byte("signed")}, nil
		default:
			t.Fatal("unexpected RPC", m)
			return control.Response{}, nil
		}
	})}
	out, err := p.Schedule(context.Background(), &pb.ScheduleRequest{StorageIntent: []byte("intent")}, nodes, func(ns []scheduler.RichNode) (scheduler.RichNode, error) { selections++; return ns[0], nil })
	if err != nil || out.GetNode().GetNodeId() != "b" || selections != 2 || len(prepares) != 2 {
		t.Fatalf("native selection lost: %+v %v %d %v", out, err, selections, prepares)
	}
}
func TestUnknownPrepareOnlyInspectsSameLaunch(t *testing.T) {
	nodes := []scheduler.RichNode{{Node: scheduler.Node{ID: "a", Endpoint: "http://a"}}}
	calls := []string{}
	p := &nativeSSFPlugin{client: fakeControl(func(_ context.Context, m string, q control.Request) (control.Response, error) {
		calls = append(calls, m)
		switch m {
		case "ResolveLaunch":
			return control.Response{LaunchID: "launch", Candidates: []control.Node{{ID: "a"}}}, nil
		case "PrepareLaunch":
			return control.Response{}, errors.New("reply lost")
		case "InspectOperation":
			if q.LaunchID != "launch" {
				t.Fatal("changed launch")
			}
			return control.Response{Selected: &control.Node{ID: "a", Endpoint: "http://a"}, Dispatch: []byte("original")}, nil
		}
		return control.Response{}, errors.New("unexpected")
	})}
	out, e := p.Schedule(context.Background(), &pb.ScheduleRequest{StorageIntent: []byte("intent")}, nodes, func(ns []scheduler.RichNode) (scheduler.RichNode, error) { return ns[0], nil })
	if e != nil || string(out.StorageDispatch) != "original" || len(calls) != 3 {
		t.Fatalf("unknown retry duplicated admission: %v %v", calls, e)
	}
}

func TestRemoteErrorCategoriesPreserveNativeGatewayMapping(t *testing.T) {
	for name, expected := range map[string]codes.Code{"unauthenticated": codes.Unauthenticated, "permission": codes.PermissionDenied, "invalid": codes.InvalidArgument, "capacity": codes.ResourceExhausted, "pending": codes.FailedPrecondition} {
		if got := status.Code(storageFailure(&control.Error{Code: name, Message: "rejected"})); got != expected {
			t.Fatalf("%s: %v", name, got)
		}
	}
	if status.Code(storageFailure(context.DeadlineExceeded)) != codes.DeadlineExceeded {
		t.Fatal("lost deadline category")
	}
}
