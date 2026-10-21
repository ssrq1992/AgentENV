package scheduler

import (
	"context"
	"errors"

	schedulerv1 "agentenv/services/api/proto"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// ControlPlugin consumes only native admission candidates and the native
// strategy callback. The plugin may filter for storage feasibility, but must
// never substitute its own node discovery or invoke the outer Schedule RPC.
type ControlPlugin interface {
	Schedule(context.Context, *schedulerv1.ScheduleRequest, []RichNode, func([]RichNode) (RichNode, error)) (*schedulerv1.ScheduleResponse, error)
	Invoke(context.Context, []byte) ([]byte, error)
}

func WithControlPlugin(p ControlPlugin) ServiceOption { return func(s *Service) { s.storage = p } }

type StorageService struct {
	schedulerv1.UnimplementedStorageControlServer
	Plugin ControlPlugin
}

func (s *StorageService) Invoke(ctx context.Context, r *schedulerv1.StorageControlRequest) (*schedulerv1.StorageControlResponse, error) {
	if s.Plugin == nil {
		return nil, status.Error(codes.Unimplemented, "storage provider disabled")
	}
	if r == nil || len(r.Envelope) == 0 || len(r.Envelope) > 1<<20 {
		return nil, status.Error(codes.InvalidArgument, "invalid storage envelope")
	}
	b, err := s.Plugin.Invoke(ctx, r.Envelope)
	if err != nil {
		return nil, err
	}
	return &schedulerv1.StorageControlResponse{Envelope: b}, nil
}

var ErrStorageDisabled = errors.New("storage intent received while provider disabled")
