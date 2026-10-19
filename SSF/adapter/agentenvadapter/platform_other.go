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
