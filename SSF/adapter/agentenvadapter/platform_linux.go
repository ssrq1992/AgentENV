//go:build linux

package agentenvadapter

import (
	"context"
	"errors"
	"fmt"
	"golang.org/x/sys/unix"
	"os"
	"os/exec"
	"path/filepath"
	"ssf.dev/ssf/core"
	"strconv"
	"strings"
	"syscall"
	"time"
	"unsafe"
)

func processStart(pid int) (string, error) {
	if pid <= 0 {
		return "", errors.New("invalid pid")
	}
	b, e := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if e != nil {
		return "", e
	}
	i := strings.LastIndexByte(string(b), ')')
	if i < 0 {
		return "", errors.New("invalid proc stat")
	}
	fields := strings.Fields(string(b[i+1:]))
	if len(fields) < 20 {
		return "", errors.New("short proc stat")
	}
	return fields[19], nil
}
func processAlive(pid int, start string) bool {
	s, e := processStart(pid)
	if e != nil || start == "" || s != start {
		return false
	}
	b, e := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if e != nil {
		return false
	}
	i := strings.LastIndexByte(string(b), ')')
	f := strings.Fields(string(b[i+1:]))
	return len(f) > 0 && f[0] != "Z"
}

// Only an explicitly stopped operation may kill its durably registered orphan.
// pidfd prevents PID reuse between identity verification and signal delivery.
func stopOrphan(ctx context.Context, ev RuntimeEvent) error {
	if !processAlive(ev.PID, ev.ProcessStart) {
		return nil
	}
	if processAlive(ev.OwnerPID, ev.OwnerStart) {
		return errors.New("native owner still running")
	}
	fd, e := unix.PidfdOpen(ev.PID, 0)
	if e != nil {
		return e
	}
	defer unix.Close(fd)
	if !processAlive(ev.PID, ev.ProcessStart) {
		return nil
	}
	if e = unix.PidfdSendSignal(fd, unix.SIGTERM, nil, 0); e != nil {
		return e
	}
	timer := time.NewTimer(3 * time.Second)
	defer timer.Stop()
	tick := time.NewTicker(50 * time.Millisecond)
	defer tick.Stop()
	for processAlive(ev.PID, ev.ProcessStart) {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-timer.C:
			if e = unix.PidfdSendSignal(fd, unix.SIGKILL, nil, 0); e != nil {
				return e
			}
		case <-tick.C:
		}
	}
	return nil
}
func verifyDevice(d Device, root string) error {
	p := filepath.Clean(d.Path)
	if !strings.HasPrefix(p, filepath.Clean(root)+string(os.PathSeparator)) {
		return errors.New("device is outside SSF publish root")
	}
	canonical, e := filepath.EvalSymlinks(p)
	if e != nil {
		return e
	}
	if canonical != p {
		return errors.New("device path contains a symlink")
	}
	var st unix.Stat_t
	if e = unix.Lstat(p, &st); e != nil {
		return e
	}
	if st.Mode&unix.S_IFMT != unix.S_IFBLK || unix.Major(st.Rdev) != d.Major || unix.Minor(st.Rdev) != d.Minor {
		return errors.New("block device identity changed")
	}
	f, e := os.Open(p)
	if e != nil {
		return e
	}
	defer f.Close()
	var size uint64
	_, _, errno := unix.Syscall(unix.SYS_IOCTL, f.Fd(), unix.BLKGETSIZE64, uintptr(unsafe.Pointer(&size)))
	if errno != 0 {
		return errno
	}
	if size != d.Bytes || size == 0 {
		return errors.New("block device capacity changed")
	}
	return nil
}
func verifyProcessDevices(ev RuntimeEvent, ds []Device) error {
	if !processAlive(ev.PID, ev.ProcessStart) {
		return errors.New("VMM process incarnation absent")
	}
	files, e := os.ReadDir(fmt.Sprintf("/proc/%d/fd", ev.PID))
	if e != nil {
		return e
	}
	for _, d := range ds {
		found := false
		for _, fd := range files {
			p := fmt.Sprintf("/proc/%d/fd/%s", ev.PID, fd.Name())
			s, e := os.Stat(p)
			if e != nil {
				continue
			}
			st, ok := s.Sys().(*syscall.Stat_t)
			if !ok || st.Mode&syscall.S_IFMT != syscall.S_IFBLK || unix.Major(uint64(st.Rdev)) != d.Major || unix.Minor(uint64(st.Rdev)) != d.Minor {
				continue
			}
			info, e := os.ReadFile(fmt.Sprintf("/proc/%d/fdinfo/%s", ev.PID, fd.Name()))
			if e != nil {
				return e
			}
			for _, line := range strings.Split(string(info), "\n") {
				if strings.HasPrefix(line, "flags:") {
					flags, e := strconv.ParseUint(strings.TrimSpace(strings.TrimPrefix(line, "flags:")), 8, 64)
					if e != nil {
						return e
					}
					if d.ReadOnly && flags&unix.O_ACCMODE != unix.O_RDONLY {
						return errors.New("VMM holds writable descriptor for readonly volume")
					}
					found = true
				}
			}
		}
		if !found {
			return fmt.Errorf("VMM has no verified descriptor for %s", d.ID)
		}
	}
	return nil
}
func commandValue(ctx context.Context, name string, args ...string) (string, error) {
	b, e := exec.CommandContext(ctx, name, args...).CombinedOutput()
	if e != nil {
		return "", fmt.Errorf("%s: %w: %s", name, e, strings.TrimSpace(string(b)))
	}
	return strings.TrimSpace(string(b)), nil
}
func (n *Node) filesystem(ctx context.Context, id string, d Device, initialize bool) (string, error) {
	typ, e := commandValue(ctx, "blkid", "-p", "-s", "TYPE", "-o", "value", d.Path)
	if e == nil {
		if typ != "ext4" {
			return "", errors.New("only ext4 guest filesystems are supported")
		}
		uuid, e := commandValue(ctx, "blkid", "-p", "-s", "UUID", "-o", "value", d.Path)
		if e != nil {
			return "", e
		}
		r, e := n.Journal.Get(id)
		if e != nil {
			return "", e
		}
		if expected := r.Format[d.ID]; expected != "" && uuid != expected {
			return "", errors.New("format intent has a different filesystem UUID")
		}
		return uuid, nil
	}
	var exit *exec.ExitError
	if !errors.As(e, &exit) || exit.ExitCode() != 2 {
		return "", e
	}
	if !initialize || d.ReadOnly {
		return "", errors.New("blank volume requires explicit new-volume initialization")
	}
	r, e := n.Journal.Get(id)
	if e != nil {
		return "", e
	}
	if r.Format[d.ID] != "" {
		return "", errors.New("previous format outcome unknown; refusing destructive retry")
	}
	f, e := os.Open(d.Path)
	if e != nil {
		return "", e
	}
	e = checkZero(ctx, f)
	_ = f.Close()
	if e != nil {
		return "", e
	}
	h := core.Hash(id + "/" + d.ID)
	uuid := h[:8] + "-" + h[8:12] + "-" + h[12:16] + "-" + h[16:20] + "-" + h[20:32]
	_, e = n.Journal.Update(id, func(x *NodeRecord) error {
		if x.Format == nil {
			x.Format = map[string]string{}
		}
		if x.Format[d.ID] != "" {
			return core.ErrConflict
		}
		x.Format[d.ID] = uuid
		return nil
	})
	if e != nil {
		return "", e
	}
	if _, e = commandValue(ctx, "mkfs.ext4", "-q", "-U", uuid, d.Path); e != nil {
		return "", e
	}
	return commandValue(ctx, "blkid", "-p", "-s", "UUID", "-o", "value", d.Path)
}

func refreshExpandedDevice(d Device, root string, target int64) (Device, error) {
	f, err := os.Open(d.Path)
	if err != nil {
		return d, err
	}
	defer f.Close()
	var size uint64
	_, _, errno := unix.Syscall(unix.SYS_IOCTL, f.Fd(), unix.BLKGETSIZE64, uintptr(unsafe.Pointer(&size)))
	if errno != 0 {
		return d, errno
	}
	if target <= 0 || size < uint64(target) {
		return d, errors.New("expanded device is smaller than authorized target")
	}
	d.Bytes = size
	return d, verifyDevice(d, root)
}

func (n *Node) expandFilesystem(ctx context.Context, id string, d Device) error {
	if d.ReadOnly || d.UUID == "" {
		return errors.New("offline expansion requires verified writable ext4")
	}
	r, err := n.Journal.Get(id)
	if err != nil {
		return err
	}
	if r.Dispatched || r.Runtime.InstanceID != "" || r.Stop {
		return core.ErrConflict
	}
	mounts, err := os.ReadFile("/proc/self/mountinfo")
	if err != nil {
		return err
	}
	device := fmt.Sprintf("%d:%d", d.Major, d.Minor)
	for _, line := range strings.Split(string(mounts), "\n") {
		fields := strings.Fields(line)
		if len(fields) > 2 && fields[2] == device {
			return errors.New("offline expansion device is mounted")
		}
	}
	// Crash replay is non-destructive: e2fsck repairs only safe preen cases;
	// resize2fs is idempotent and never creates or replaces a filesystem.
	if _, err = commandValue(ctx, "e2fsck", "-p", d.Path); err != nil {
		var exit *exec.ExitError
		if !errors.As(err, &exit) || exit.ExitCode() != 1 {
			return err
		}
	}
	if _, err = commandValue(ctx, "resize2fs", d.Path); err != nil {
		return err
	}
	uuid, err := commandValue(ctx, "blkid", "-p", "-s", "UUID", "-o", "value", d.Path)
	if err != nil {
		return err
	}
	if uuid != d.UUID {
		return errors.New("filesystem identity changed during expansion")
	}
	return verifyDevice(d, n.Config.DeviceRoot)
}
