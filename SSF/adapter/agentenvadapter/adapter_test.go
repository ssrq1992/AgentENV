package agentenvadapter

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"errors"
	"github.com/container-storage-interface/spec/lib/go/csi"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"ssf.dev/ssf/auth"
	"ssf.dev/ssf/core"
	"sync"
	"testing"
	"time"
)

func validRequest() CreateRequest {
	return CreateRequest{RequestID: "request-1", Kind: "cold", Launch: json.RawMessage(`{"image":"example/root@sha256:abc"}`), Mounts: []Mount{{Namespace: "tenant-a", PVC: "data", ID: "data", Path: "/data"}}}
}
func TestRequestAuthorizationAndReservedFields(t *testing.T) {
	p := Principal{Tenant: "a", Namespaces: []string{"tenant-a"}}
	q := validRequest()
	if e := q.Validate(p); e != nil {
		t.Fatal(e)
	}
	for _, mutation := range []func(*CreateRequest){func(q *CreateRequest) { q.Mounts[0].Namespace = "tenant-b" }, func(q *CreateRequest) { q.Mounts[0].Path = "/data/../etc" }, func(q *CreateRequest) { q.Mounts[0].Path = "/dev/data" }, func(q *CreateRequest) { q.Launch = json.RawMessage(`{"image":"x","autoPause":true}`) }, func(q *CreateRequest) {
		q.Launch = json.RawMessage(`{"image":"x","customExtensionParams":{"ticket":"forged"}}`)
	}, func(q *CreateRequest) {
		q.Mounts = append(q.Mounts, Mount{Namespace: "tenant-a", PVC: "other", ID: "other", Path: "/data/nested"})
	}} {
		q := validRequest()
		mutation(&q)
		if q.Validate(p) == nil {
			t.Fatalf("accepted unsafe request: %+v", q)
		}
	}
}
func TestTemplateSlotsAndResourcePinning(t *testing.T) {
	c := Control{Config: Config{Templates: map[string]Template{"immutable": {ID: "immutable", Resources: Resources{2, 1024}, RWSlots: 1}}}}
	q := validRequest()
	q.Kind = "template"
	q.Launch = json.RawMessage(`{"templateID":"immutable"}`)
	res, e := c.ResolveLaunch(&q)
	if e != nil || res.MemoryMiB != 1024 {
		t.Fatalf("%+v %v", res, e)
	}
	q.Mounts[0].ReadOnly = true
	if _, e = c.ResolveLaunch(&q); e == nil {
		t.Fatal("readonly volume admitted into writable template slot")
	}
	q.Kind = "cold"
	q.Launch = json.RawMessage(`{"image":"x"}`)
	res, e = c.ResolveLaunch(&q)
	if e != nil || res.CPU != 2 {
		t.Fatal(res, e)
	}
	var b map[string]any
	_ = json.Unmarshal(q.Launch, &b)
	if b["memoryMB"] != float64(512) {
		t.Fatal("reservation defaults not sent to native API")
	}
}
func testNode(t *testing.T) (*Node, StartRequest) {
	t.Helper()
	pub, key, e := ed25519.GenerateKey(rand.Reader)
	if e != nil {
		t.Fatal(e)
	}
	j, e := OpenJournal(t.TempDir())
	if e != nil {
		t.Fatal(e)
	}
	t.Cleanup(func() { j.Close() })
	n := &Node{Journal: j, Key: pub, Config: Config{NodeName: "n", NodeUID: "uid", RuntimeRoot: t.TempDir()}, BootID: "boot", Context: context.Background()}
	p := core.Plan{ID: "plan", Operation: "consume-op", Sandbox: "instance", Node: "n", NodeUID: "uid", BootID: "boot", Expires: time.Now().Add(time.Minute), Bindings: []core.Binding{{ID: "b", Volume: core.Volume{Capability: &csi.VolumeCapability{AccessType: &csi.VolumeCapability_Block{Block: &csi.VolumeCapability_BlockVolume{}}}}}}}
	token, e := auth.Sign(p, key)
	if e != nil {
		t.Fatal(e)
	}
	q := StartRequest{ID: "op", Instance: "instance", Operation: "consume-op", Plan: token, Kind: "cold", Launch: json.RawMessage(`{"image":"x"}`)}
	return n, q
}
func TestDurableCancelPreventsLateLaunch(t *testing.T) {
	n, q := testNode(t)
	r, e := n.Register(q, true)
	if e != nil {
		t.Fatal(e)
	}
	if e = n.stop(context.Background(), r); e != nil {
		t.Fatal(e)
	}
	r, e = n.Register(q, false)
	if e != nil || !r.Stop {
		t.Fatalf("cancel lost: %+v %v", r, e)
	}
	if e = n.Work(context.Background(), q.ID); e != nil {
		t.Fatal(e)
	}
	r, _ = n.Journal.Get(q.ID)
	if r.Dispatched || r.State != "Stopped" {
		t.Fatal(r)
	}
	if e = n.verifyRuntime(r, "detached"); e != nil {
		t.Fatal(e)
	}
	q.Launch = json.RawMessage(`{"image":"different"}`)
	if _, e = n.Register(q, false); !errors.Is(e, core.ErrConflict) {
		t.Fatal("conflicting request accepted", e)
	}
}
func TestUnknownNativeCreateRetainsResources(t *testing.T) {
	n, q := testNode(t)
	_, e := n.Register(q, false)
	if e != nil {
		t.Fatal(e)
	}
	r, e := n.Journal.Update(q.ID, func(r *NodeRecord) error { r.Dispatched = true; r.Stop = true; return nil })
	if e != nil {
		t.Fatal(e)
	}
	if e = n.stop(context.Background(), r); e == nil {
		t.Fatal("unknown dispatched create considered stopped")
	}
	r, _ = n.Journal.Get(q.ID)
	if r.State == "Stopped" {
		t.Fatal("unknown outcome discarded")
	}
}
func TestNativeInstanceBindingAndEventOrdering(t *testing.T) {
	n, q := testNode(t)
	r, e := n.Register(q, false)
	if e != nil {
		t.Fatal(e)
	}
	ev := RuntimeEvent{Sequence: 1, Ticket: r.Ticket, SandboxID: "sandbox", InstanceID: "native-1", BootID: "boot", Event: "prepare"}
	if e = n.acceptEvent(ev); e != nil {
		t.Fatal(e)
	}
	old := ev
	ev.InstanceID = "native-2"
	if e = n.acceptEvent(ev); e == nil {
		t.Fatal("duplicate native instance accepted")
	}
	ev = old
	ev.Event = "stopped"
	ev.Sequence = 2
	data, _ := json.Marshal(ev)
	if e = os.WriteFile(filepath.Join(n.Config.RuntimeRoot, ev.InstanceID+".json"), data, 0600); e != nil {
		t.Fatal(e)
	}
	if e = n.acceptEvent(ev); e != nil {
		t.Fatal(e)
	}
	if e = n.acceptEvent(old); e != nil {
		t.Fatal(e)
	}
	r, _ = n.Journal.Get(q.ID)
	if r.Runtime.Event != "stopped" {
		t.Fatal("late prepare resurrected stopped runtime")
	}
	if e = n.verifyRuntime(r, "detached"); e != nil {
		t.Fatal(e)
	}
}
func TestJournalConcurrentUpdatesAndRestart(t *testing.T) {
	root := t.TempDir()
	j, e := OpenJournal(root)
	if e != nil {
		t.Fatal(e)
	}
	_, e = j.Update("a", func(r *NodeRecord) error { r.Ticket = "secret"; r.Format = map[string]string{}; return nil })
	if e != nil {
		t.Fatal(e)
	}
	var wg sync.WaitGroup
	for i := 0; i < 30; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_, e := j.Update("a", func(r *NodeRecord) error { r.Runtime.Sequence++; return nil })
			if e != nil {
				t.Error(e)
			}
		}()
	}
	wg.Wait()
	j.Close()
	j, e = OpenJournal(root)
	if e != nil {
		t.Fatal(e)
	}
	defer j.Close()
	r, e := j.ByTicket("secret")
	if e != nil || r.Runtime.Sequence != 30 {
		t.Fatalf("durability: %+v %v", r, e)
	}
}
func TestRuntimeAdapterDoesNotClaimUnsupportedCapabilities(t *testing.T) {
	n, q := testNode(t)
	_, _ = n.Register(q, false)
	a := &RuntimeAdapter{Node: n, Operation: q.ID}
	caps, e := a.Capabilities(context.Background())
	if e != nil || !caps.BlockDevice || caps.FileTree || caps.Freeze {
		t.Fatal(caps, e)
	}
	if a.Freeze(context.Background(), q.Instance) == nil {
		t.Fatal("freeze implemented by assertion")
	}
	if _, e = a.InspectBindings(context.Background(), q.Instance); e == nil {
		t.Fatal("unknown runtime treated as absent")
	}
}

func TestRebootAndPodReplacementEvidence(t *testing.T) {
	n, q := testNode(t)
	r, e := n.Register(q, false)
	if e != nil {
		t.Fatal(e)
	}
	n.PIDNamespace = "different"
	if n.Work(context.Background(), q.ID) == nil {
		t.Fatal("PID namespace replacement silently accepted")
	}
	if n.verifyRuntime(r, "detached") == nil {
		t.Fatal("namespace absence treated as fencing")
	}
	n.BootID = "new-boot"
	if e = n.Work(context.Background(), q.ID); e != nil {
		t.Fatal(e)
	}
	r, _ = n.Journal.Get(q.ID)
	if !r.Stop || n.verifyRuntime(r, "detached") != nil {
		t.Fatal("host reboot not recovered")
	}
	n.Config.NodeUID = "replacement"
	if e = n.Work(context.Background(), q.ID); e == nil {
		t.Fatal("node UID reuse accepted")
	}
}
func TestSameTenantAuthorizationMustBeDeterministic(t *testing.T) {
	c := Config{Cert: "x", Key: "x", CA: "x", DatabaseURL: "postgres", Workers: []Worker{{Name: "n", URL: "https://n", Resources: Resources{2, 512}}}, Principals: map[string]Principal{
		"spiffe://ssf/one": {Tenant: "a", Namespaces: []string{"one"}}, "spiffe://ssf/two": {Tenant: "a", Namespaces: []string{"two"}}}}
	if c.Validate("control") == nil {
		t.Fatal("ambiguous tenant ACL")
	}
	c.Principals["spiffe://ssf/two"] = c.Principals["spiffe://ssf/one"]
	if e := c.Validate("control"); e != nil {
		t.Fatal(e)
	}
}

func TestGuestProxyNeverRoutesNativeManagement(t *testing.T) {
	for _, p := range []string{"/sandboxes", "/sandboxes/a/pause", "/process.Process/../sandboxes", "/process.Process/Start/extra", "/process.Process/%2e%2e"} {
		if guestPath(p) {
			t.Fatalf("unsafe guest proxy path %s", p)
		}
	}
	if !guestPath("/process.Process/Start") || !guestPath("/filesystem.Filesystem/Stat") {
		t.Fatal("known guest service rejected")
	}
}

func TestLostInitialCallbackRecoveredFromNativeJournal(t *testing.T) {
	n, q := testNode(t)
	r, e := n.Register(q, false)
	if e != nil {
		t.Fatal(e)
	}
	_, e = n.Journal.Update(q.ID, func(x *NodeRecord) error { x.Dispatched = true; return nil })
	if e != nil {
		t.Fatal(e)
	}
	ev := RuntimeEvent{Sequence: 2, Ticket: r.Ticket, SandboxID: "sandbox", InstanceID: "native", BootID: n.BootID, Event: "stopped"}
	b, _ := json.Marshal(ev)
	if e = os.WriteFile(filepath.Join(n.Config.RuntimeRoot, "native.json"), b, 0600); e != nil {
		t.Fatal(e)
	}
	if e = n.Work(context.Background(), q.ID); e != nil {
		t.Fatal(e)
	}
	r, _ = n.Journal.Get(q.ID)
	if r.State != "Stopped" || r.Runtime.InstanceID != "native" {
		t.Fatal("lost notification not recovered", r)
	}
}
func TestRevocationBeforeDelayedPlan(t *testing.T) {
	n, q := testNode(t)
	_, e := n.Journal.Update(q.ID, func(r *NodeRecord) error {
		*r = NodeRecord{Request: StartRequest{ID: q.ID, Instance: q.Instance}, Ticket: newID(), State: "Revoked", Stop: true}
		return nil
	})
	if e != nil {
		t.Fatal(e)
	}
	r, e := n.Register(q, false)
	if e != nil || !r.Stop {
		t.Fatal("late start bypassed revocation", e)
	}
	if e = n.Work(context.Background(), q.ID); e != nil {
		t.Fatal(e)
	}
	r, _ = n.Journal.Get(q.ID)
	if r.Dispatched || r.State != "Stopped" {
		t.Fatal(r)
	}
}
func TestPlanRefreshCannotChangeBindings(t *testing.T) {
	n, q := testNode(t)
	p, e := auth.Verify(q.Plan, n.Key, n.Config.NodeName, n.BootID, time.Now())
	if e != nil {
		t.Fatal(e)
	}
	pub, key, _ := ed25519.GenerateKey(rand.Reader)
	n.Key = pub
	q.Plan, _ = auth.Sign(p, key)
	if _, e = n.Register(q, false); e != nil {
		t.Fatal(e)
	}
	p.Expires = time.Now().Add(2 * time.Minute)
	q.Plan, _ = auth.Sign(p, key)
	if _, e = n.Register(q, false); e != nil {
		t.Fatal("same plan renewal rejected", e)
	}
	p.Bindings[0].Volume.Handle = "different"
	q.Plan, _ = auth.Sign(p, key)
	if _, e = n.Register(q, false); !errors.Is(e, core.ErrConflict) {
		t.Fatal("plan replacement accepted", e)
	}
}

func TestInitializeRequiresEntireDeviceEmpty(t *testing.T) {
	if e := checkZero(context.Background(), bytes.NewReader(make([]byte, 2<<20))); e != nil {
		t.Fatal(e)
	}
	b := make([]byte, (2<<20)+1)
	b[len(b)-1] = 1
	if checkZero(context.Background(), bytes.NewReader(b)) == nil {
		t.Fatal("unrecognized existing data would be formatted")
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if !errors.Is(checkZero(ctx, bytes.NewReader(nil)), context.Canceled) {
		t.Fatal("cancelled scan accepted")
	}
}

func TestPeerIdentityRequiresVerifiedSingleURI(t *testing.T) {
	req := httptest.NewRequest("GET", "https://node/v1/info", nil)
	n, _ := testNode(t)
	n.Config.ControlPeers = []string{"spiffe://ssf/control"}
	w := httptest.NewRecorder()
	n.ServeHTTP(w, req)
	if w.Code != 403 {
		t.Fatal("unauthenticated peer admitted", w.Code)
	}
	uri, _ := url.Parse("spiffe://ssf/control")
	req.TLS = &tls.ConnectionState{PeerCertificates: []*x509.Certificate{{URIs: []*url.URL{uri}}}}
	if identity(req) != "" {
		t.Fatal("unverified certificate admitted")
	}
	req.TLS.VerifiedChains = [][]*x509.Certificate{{{URIs: []*url.URL{uri, uri}}}}
	if identity(req) != "" {
		t.Fatal("ambiguous peer identity admitted")
	}
}
func TestGuestProxyOverridesCallerRouting(t *testing.T) {
	backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("x-agentenv-sandbox-id") != "fixed" || r.Header.Get("x-agentenv-target-port") != "49983" || r.Header.Get("X-API-Key") != "server-only" || r.Header.Get("X-Access-Token") != "guest-token" || r.URL.Path != "/process.Process/Start" {
			t.Error("routing or credentials not fixed")
		}
		w.WriteHeader(204)
	}))
	defer backend.Close()
	req := httptest.NewRequest("POST", "https://adapter/guest?target=evil", nil)
	req.Header.Set("x-agentenv-sandbox-id", "forged")
	req.Header.Set("X-API-Key", "forged")
	w := httptest.NewRecorder()
	forwardGuest(w, req, &HTTPClient{Client: backend.Client(), APIKey: "server-only"}, backend.URL, "/process.Process/Start", "fixed", "guest-token")
	if w.Code != 204 {
		t.Fatal(w.Code, w.Body.String())
	}
}
