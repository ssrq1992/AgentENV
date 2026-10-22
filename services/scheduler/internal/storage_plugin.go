package scheduler

import (
	"context"
	"errors"

	schedulerv1 "agentenv/services/api/proto"
)

// ControlPlugin consumes only native admission candidates and the native
// strategy callback. The plugin may filter for storage feasibility, but must
// never substitute its own node discovery or invoke the outer Schedule RPC.
type ControlPlugin interface {
	Schedule(context.Context, *schedulerv1.ScheduleRequest, []RichNode, func([]RichNode) (RichNode, error)) (*schedulerv1.ScheduleResponse, error)
}

func WithControlPlugin(p ControlPlugin) ServiceOption { return func(s *Service) { s.storage = p } }

var ErrStorageDisabled = errors.New("storage intent received while provider disabled")
