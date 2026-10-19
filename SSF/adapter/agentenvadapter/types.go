// Package agentenvadapter integrates AgentENV without adding project logic to SSF.
package agentenvadapter

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"path"
	"regexp"
	"strings"
	"time"

	"ssf.dev/ssf/consume"
	"ssf.dev/ssf/core"
)

var ErrUnsupported = errors.New("operation is not supported for externally managed volumes")
var safeID = regexp.MustCompile(`^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}$`)

type Mount struct {
	Namespace string `json:"namespace"`
	PVC       string `json:"pvc"`
	UID       string `json:"uid,omitempty"`
	ID        string `json:"id"`
	Path      string `json:"path"`
	ReadOnly  bool   `json:"readonly"`
}
type CreateRequest struct {
	RequestID string          `json:"request_id"`
	Kind      string          `json:"kind"` // cold or template
	Launch    json.RawMessage `json:"launch"`
	Mounts    []Mount         `json:"mounts"`
}
type Resources struct {
	CPU       int64 `json:"cpu"`
	MemoryMiB int64 `json:"memory_mib"`
}
type Principal struct {
	Tenant     string   `json:"tenant"`
	Namespaces []string `json:"namespaces"`
}
type Operation struct {
	Timings   map[string]float64 `json:"timings_ms"`
	ID        string             `json:"id"`
	Tenant    string             `json:"tenant"`
	Instance  string             `json:"instance"`
	Request   CreateRequest      `json:"request"`
	Resources Resources          `json:"resources"`
	Result    consume.Result     `json:"-"`
	State     string             `json:"state"`
	Reason    string             `json:"reason,omitempty"`
	Stop      bool               `json:"stopping"`
	SandboxID string             `json:"sandbox_id,omitempty"`
	Native    json.RawMessage    `json:"sandbox,omitempty"`
	Updated   time.Time          `json:"updated"`
}
type VolumeRequest struct {
	Namespace      string `json:"namespace"`
	Name           string `json:"name"`
	StorageClass   string `json:"storage_class"`
	Bytes          int64  `json:"bytes"`
	InitializeExt4 bool   `json:"initialize_ext4"`
}
type StartRequest struct {
	ID         string          `json:"id"`
	Instance   string          `json:"instance"`
	Operation  string          `json:"operation"`
	Plan       string          `json:"plan"`
	Kind       string          `json:"kind"`
	Launch     json.RawMessage `json:"launch"`
	Initialize map[string]bool `json:"initialize"` // binding UID; control-authorized new claims only
	Resources  Resources       `json:"resources"`
}

// Device is the generic AgentENV external-storage contract. It contains no CSI concepts.
type Device struct {
	ID        string `json:"id"`
	Path      string `json:"path"`
	GuestPath string `json:"guest_path"`
	ReadOnly  bool   `json:"readonly"`
	Major     uint32 `json:"major"`
	Minor     uint32 `json:"minor"`
	Bytes     uint64 `json:"bytes"`
	UUID      string `json:"uuid"`
}
type RuntimeEvent struct {
	Sequence     uint64   `json:"sequence"`
	Ticket       string   `json:"ticket"`
	SandboxID    string   `json:"sandbox_id"`
	InstanceID   string   `json:"instance_id"`
	Event        string   `json:"event"`
	OwnerPID     int      `json:"owner_pid"`
	OwnerStart   string   `json:"owner_start"`
	PID          int      `json:"pid"`
	ProcessStart string   `json:"process_start"`
	BootID       string   `json:"boot_id"`
	Devices      []Device `json:"devices,omitempty"`
}
type NodeRecord struct {
	Timings      map[string]float64 `json:"timings_ms"`
	LaunchAt     time.Time          `json:"launch_at"`
	Request      StartRequest       `json:"request"`
	State        string             `json:"state"`
	Reason       string             `json:"reason,omitempty"`
	Ticket       string             `json:"ticket"`
	Stop         bool               `json:"stop"`
	Dispatched   bool               `json:"dispatched"`
	Response     json.RawMessage    `json:"response,omitempty"`
	Runtime      RuntimeEvent       `json:"runtime"`
	Devices      []Device           `json:"devices"`
	Format       map[string]string  `json:"format"`
	BootID       string             `json:"boot_id"`
	NodeUID      string             `json:"node_uid"`
	PIDNamespace string             `json:"pid_namespace"`
	Updated      time.Time          `json:"updated"`
}

func newID() string {
	var b [24]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(err)
	}
	return hex.EncodeToString(b[:])
}
func (p Principal) Allows(ns string) bool {
	for _, v := range p.Namespaces {
		if ns == v {
			return true
		}
	}
	return false
}
func (q CreateRequest) Validate(p Principal) error {
	if !safeID.MatchString(q.RequestID) || (q.Kind != "cold" && q.Kind != "template") {
		return errors.New("request_id and kind cold/template required")
	}
	var body map[string]json.RawMessage
	if err := json.Unmarshal(q.Launch, &body); err != nil || body == nil {
		return errors.New("launch must be an object")
	}
	for _, k := range []string{"volumeMounts", "customExtensionParams", "attachedDrives"} {
		if _, ok := body[k]; ok {
			return fmt.Errorf("launch.%s is reserved in this adapter", k)
		}
	}
	for _, k := range []string{"autoPause", "autoResume"} {
		if v, ok := body[k]; ok && string(v) != "false" && string(v) != "null" {
			return fmt.Errorf("%s: %w", k, ErrUnsupported)
		}
	}
	var source string
	key := "image"
	if q.Kind == "template" {
		key = "templateID"
	}
	if json.Unmarshal(body[key], &source) != nil || source == "" {
		return fmt.Errorf("launch.%s required", key)
	}
	if len(q.Mounts) > 8 {
		return errors.New("at most 8 mounts are supported")
	}
	ms := q.CoreMounts()
	if err := core.ValidateMounts(ms); err != nil {
		return err
	}
	for _, m := range q.Mounts {
		for _, other := range q.Mounts {
			if other.ID != m.ID && strings.HasPrefix(m.Path, other.Path+"/") {
				return errors.New("overlapping guest mount paths")
			}
		}
		if !p.Allows(m.Namespace) || !safeID.MatchString(m.ID) {
			return errors.New("mount namespace or id forbidden")
		}
		if strings.ContainsAny(m.Path, " \t\n\r,:\\'\"$`;()") || path.Clean(m.Path) != m.Path {
			return errors.New("unsafe guest path")
		}
		for _, prefix := range []string{"/dev", "/proc", "/sys", "/agentenv", "/run"} {
			if m.Path == prefix || strings.HasPrefix(m.Path, prefix+"/") {
				return errors.New("reserved guest path")
			}
		}
	}
	return nil
}
func (q CreateRequest) CoreMounts() []core.Mount {
	out := make([]core.Mount, 0, len(q.Mounts))
	for _, m := range q.Mounts {
		out = append(out, core.Mount{PVC: core.PVCRef{Namespace: m.Namespace, Name: m.PVC, UID: m.UID}, MountID: m.ID, GuestPath: m.Path, ReadOnly: m.ReadOnly})
	}
	return out
}
func launchBody(q StartRequest, ticket string) ([]byte, error) {
	var b map[string]any
	if e := json.Unmarshal(q.Launch, &b); e != nil {
		return nil, e
	}
	b["autoPause"] = false
	delete(b, "autoResume")
	b["customExtensionParams"] = map[string]any{"external_storage": map[string]string{"ticket": ticket}}
	return json.Marshal(b)
}
