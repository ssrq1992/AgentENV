package agentenvadapter

import (
	"context"
	"errors"
	"ssf.dev/ssf/core"
	"ssf.dev/ssf/runtime"
	"time"
)

// RuntimeAdapter exposes the public SSF contract over an already registered
// node operation. It never creates a second owner of the prepared devices.
type RuntimeAdapter struct {
	Node      *Node
	Operation string
}

var _ runtime.Adapter = (*RuntimeAdapter)(nil)

func (a *RuntimeAdapter) Capabilities(context.Context) (runtime.Capabilities, error) {
	return runtime.Capabilities{BlockDevice: true}, nil
}
func (a *RuntimeAdapter) AttachPrepared(_ context.Context, instance string, eps []core.Endpoint) error {
	r, e := a.Node.Journal.Get(a.Operation)
	if e != nil {
		return e
	}
	if instance != r.Request.Instance || len(eps) != len(r.Devices) {
		return core.ErrConflict
	}
	for i, d := range r.Devices {
		if eps[i].Kind != "BlockDevice" || eps[i].BindingID != d.ID || eps[i].Path != d.Path || eps[i].Major != d.Major || eps[i].Minor != d.Minor || eps[i].Size != d.Bytes || eps[i].ReadOnly != d.ReadOnly || eps[i].GuestPath != d.GuestPath {
			return core.ErrConflict
		}
	}
	a.Node.Kick(a.Operation)
	return nil
}
func (a *RuntimeAdapter) WaitGuestReady(ctx context.Context, instance string) error {
	return a.wait(ctx, instance, "ready")
}
func (a *RuntimeAdapter) wait(ctx context.Context, instance, action string) error {
	t := time.NewTicker(100 * time.Millisecond)
	defer t.Stop()
	for {
		r, e := a.Node.Journal.Get(a.Operation)
		if e != nil {
			return e
		}
		if r.Request.Instance != instance {
			return core.ErrConflict
		}
		if a.Node.verifyRuntime(r, action) == nil {
			return nil
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-t.C:
		}
	}
}
func (a *RuntimeAdapter) DetachAndConfirm(ctx context.Context, instance string) error {
	r, e := a.Node.Journal.Update(a.Operation, func(r *NodeRecord) error {
		if r.Request.Instance != instance {
			return core.ErrConflict
		}
		r.Stop = true
		return nil
	})
	if e != nil {
		return e
	}
	if e = a.Node.stop(ctx, r); e != nil {
		return e
	}
	return a.wait(ctx, instance, "detached")
}
func (a *RuntimeAdapter) InspectBindings(_ context.Context, instance string) ([]string, error) {
	r, e := a.Node.Journal.Get(a.Operation)
	if e != nil {
		return nil, e
	}
	if r.Request.Instance != instance {
		return nil, core.ErrConflict
	}
	if r.State == "Released" {
		return nil, nil
	}
	if r.Runtime.InstanceID == "" {
		return nil, errors.New("runtime launch outcome not yet confirmed")
	}
	var out []string
	for _, d := range r.Devices {
		out = append(out, d.ID)
	}
	return out, nil
}
func (a *RuntimeAdapter) Freeze(context.Context, string) error { return runtime.ErrUnsupported }
func (a *RuntimeAdapter) Thaw(context.Context, string) error   { return runtime.ErrUnsupported }
