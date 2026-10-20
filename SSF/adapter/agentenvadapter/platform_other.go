//go:build !linux

package agentenvadapter

import "context"

func stopOrphan(context.Context, RuntimeEvent) error { return ErrUnsupported }

func processAlive(int, string) bool                     { return false }
func verifyDevice(Device, string) error                 { return ErrUnsupported }
func verifyProcessDevices(RuntimeEvent, []Device) error { return ErrUnsupported }
func (n *Node) filesystem(context.Context, string, Device, bool) (string, error) {
	return "", ErrUnsupported
}

func refreshExpandedDevice(d Device, _ string, _ int64) (Device, error) { return d, ErrUnsupported }
func (n *Node) expandFilesystem(context.Context, string, Device) error  { return ErrUnsupported }
