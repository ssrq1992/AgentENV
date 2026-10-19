package agentenvadapter

import (
	"context"
	"crypto/ed25519"
	"encoding/json"
	"errors"
	"fmt"
	v1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"log/slog"
	"net/http"
	"ssf.dev/ssf/auth"
	"ssf.dev/ssf/consume"
	"ssf.dev/ssf/core"
	kube "ssf.dev/ssf/kubernetes"
	"ssf.dev/ssf/ledger"
	"ssf.dev/ssf/observe"
	"strings"
	"sync"
	"time"
)

type namespaceAuth struct{ Principal Principal }

func (a namespaceAuth) Authorize(_ context.Context, tenant string, ref core.PVCRef, _ bool) error {
	if tenant != a.Principal.Tenant || !a.Principal.Allows(ref.Namespace) {
		return errors.New("PVC not authorized")
	}
	return nil
}

type Control struct {
	Config  Config
	Store   *Store
	Ledger  *ledger.Store
	Kube    *kube.Client
	Drivers consume.Controllers
	Key     ed25519.PrivateKey
	HTTP    *HTTPClient
	Metrics *observe.Registry
	Context context.Context
	running sync.Map
}

func (c *Control) principal(tenant string) (Principal, error) {
	for _, p := range c.Config.Principals {
		if p.Tenant == tenant {
			return p, nil
		}
	}
	return Principal{}, errors.New("tenant authorization removed")
}
func (c *Control) coordinator(o Operation) (*consume.Coordinator, *Placement, error) {
	p, e := c.principal(o.Tenant)
	if e != nil {
		return nil, nil, e
	}
	place := &Placement{Store: c.Store, Workers: c.Config.Workers, HTTP: c.HTTP, Kube: c.Kube, Resources: o.Resources}
	return &consume.Coordinator{Kube: c.Kube, Ledger: c.Ledger, Placement: place, Drivers: c.Drivers, Authorizer: namespaceAuth{p}, SigningKey: c.Key, PlanTTL: 5 * time.Minute}, place, nil
}
func (c *Control) ResolveLaunch(q *CreateRequest) (Resources, error) {
	var b map[string]json.RawMessage
	if e := json.Unmarshal(q.Launch, &b); e != nil {
		return Resources{}, e
	}
	var res Resources
	if q.Kind == "cold" {
		res = Resources{CPU: 2, MemoryMiB: 512}
		if v := b["cpuCount"]; v != nil {
			if e := json.Unmarshal(v, &res.CPU); e != nil {
				return res, e
			}
		}
		if v := b["memoryMB"]; v != nil {
			if e := json.Unmarshal(v, &res.MemoryMiB); e != nil {
				return res, e
			}
		}
		b["cpuCount"], _ = json.Marshal(res.CPU)
		b["memoryMB"], _ = json.Marshal(res.MemoryMiB)
	} else {
		var id string
		_ = json.Unmarshal(b["templateID"], &id)
		t, ok := c.Config.Templates[id]
		if !ok || t.ID != id {
			return res, errors.New("template must be an immutable ID in the verified template catalog")
		}
		res = t.Resources
		rw, ro := 0, 0
		for _, m := range q.Mounts {
			if m.ReadOnly {
				ro++
			} else {
				rw++
			}
		}
		if rw > t.RWSlots || ro > t.ROSlots {
			return res, errors.New("template lacks compatible external-drive slots")
		}
	}
	if res.CPU <= 0 || res.MemoryMiB < 128 {
		return res, errors.New("invalid CPU or memory request")
	}
	var e error
	q.Launch, e = json.Marshal(b)
	return res, e
}
func (c *Control) Kick(id string) {
	if _, loaded := c.running.LoadOrStore(id, true); loaded {
		return
	}
	go func() {
		defer c.running.Delete(id)
		ctx, cancel := context.WithTimeout(c.Context, 2*time.Minute)
		defer cancel()
		start := time.Now()
		e := c.Step(ctx, id)
		if c.Metrics != nil {
			c.Metrics.Observe("adapter_control", time.Since(start), e)
		}
		if e != nil && !errors.Is(e, core.ErrPending) {
			slog.Warn("adapter operation pending", "operation", id, "error", e)
		}
	}()
}
func (c *Control) Recover(ctx context.Context) {
	ticker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()
	for {
		ids, e := c.Store.Pending(ctx)
		if e == nil {
			for _, id := range ids {
				c.Kick(id)
			}
		}
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
	}
}
func (c *Control) Step(ctx context.Context, id string) error {
	return c.Store.Exclusive(ctx, id, func() error {
		o, e := c.Store.Get(ctx, id)
		if e != nil {
			return e
		}
		if o.State == "Released" || o.State == "Rejected" {
			return nil
		}
		if o.Timings == nil {
			o.Timings = map[string]float64{}
		}
		for _, k := range []string{"reservation_ms", "authorization_ms", "attach_ms"} {
			if _, ok := o.Timings[k]; !ok {
				o.Timings[k] = 0
			}
		}
		cdr, place, e := c.coordinator(o)
		if e != nil {
			return e
		}
		cdr.Placement = timedPlacement{place, o.Timings}
		cdr.Authorizer = timedAuth{cdr.Authorizer, o.Timings}
		cdr.Drivers = timedDrivers{cdr.Drivers, o.Timings}
		defer func() {
			if e != nil {
				o.Reason = e.Error()
				_ = c.Store.Save(context.WithoutCancel(ctx), o)
			}
		}()
		if o.Result.SignedPlan == "" {
			if o.Stop {
				op := core.Hash(o.Tenant + "\x00" + o.Instance + "\x00" + o.ID)
				e = cdr.AbortUnpublished(ctx, op, func(ctx context.Context) error {
					current, err := c.Store.Get(ctx, o.ID)
					if err != nil {
						return err
					}
					if !current.Stop || current.Result.SignedPlan != "" {
						return core.ErrConflict
					}
					return nil
				})
				if e != nil {
					return e
				}
				if e = place.Release(ctx, core.Placement{ID: op}); e != nil {
					return e
				}
				o.State = "Released"
				return c.Store.Save(ctx, o)
			}
			o.Result, e = cdr.Prepare(ctx, consume.Request{ID: o.ID, Tenant: o.Tenant, Sandbox: o.Instance, Mounts: o.Request.CoreMounts()})
			if save := c.Store.Save(context.WithoutCancel(ctx), o); save != nil {
				return save
			}
			if e != nil {
				return e
			}
			o.State = "Planned"
		}
		latest, err := c.Store.Get(ctx, o.ID)
		if err != nil {
			return err
		}
		o.Stop = latest.Stop
		var worker Worker
		for _, n := range c.Config.Workers {
			if n.Name == o.Result.Placement.Node {
				worker = n
			}
		}
		if worker.URL == "" {
			return errors.New("reserved worker removed; restore its configuration to recover")
		}
		// Probe existence before renewing an expired, not-yet-delivered plan.
		var nr NodeRecord
		e = c.HTTP.Call(ctx, "GET", base(worker.URL)+"/v1/operations/"+o.ID, nil, &nr)
		var he *HTTPError
		absent := errors.As(e, &he) && he.Status == 404 || e == nil && nr.Request.Plan == ""
		refresh := (absent || e == nil && !nr.Dispatched) && time.Now().After(o.Result.Plan.Expires)
		if refresh {
			placement, err := place.Renew(ctx, o.Result.Placement)
			if err != nil {
				return err
			}
			o.Result.Placement = placement
			o.Result.Plan.Expires = time.Now().Add(4 * time.Minute)
			o.Result.SignedPlan, e = auth.Sign(o.Result.Plan, c.Key)
			if e != nil {
				return e
			}
			if e = c.Store.Save(ctx, o); e != nil {
				return e
			}
		}
		if absent || refresh {
			init := map[string]bool{}
			for _, b := range o.Result.Plan.Bindings {
				if !b.Volume.ReadOnly {
					init[b.ID], e = c.Store.AuthorizeInitialization(ctx, b.Volume.PVC, o.ID)
					if e != nil {
						return e
					}
				}
			}
			q := StartRequest{ID: o.ID, Instance: o.Instance, Operation: o.Result.Operation, Plan: o.Result.SignedPlan, Kind: o.Request.Kind, Launch: o.Request.Launch, Initialize: init, Resources: o.Resources}
			route := "/v1/start"
			if o.Stop {
				route = "/v1/cancel-unstarted"
			}
			e = c.HTTP.Call(ctx, "POST", base(worker.URL)+route, q, &nr)
		}
		if e != nil {
			return e
		}
		for k, v := range nr.Timings {
			o.Timings[k] = v
		}
		o.SandboxID = nr.Runtime.SandboxID
		o.Native = nr.Response
		// A timeout-triggered native deletion also enters the same release path.
		if nr.State == "Failed" || nr.State == "Stopped" || nr.State == "Released" {
			o.Stop = true
			if e = c.Store.Stop(ctx, o.ID, o.Tenant); e != nil {
				return e
			}
		}
		if o.Stop {
			if nr.State != "Released" {
				e = c.HTTP.Call(ctx, "POST", base(worker.URL)+"/v1/operations/"+o.ID+"/stop", struct{}{}, &nr)
				if e != nil {
					return e
				}
			}
			if nr.State != "Stopped" && nr.State != "Released" {
				o.State = "Stopping"
				e = c.Store.Save(ctx, o)
				return e
			}
			e = cdr.Release(ctx, o.Result.Operation, func(ctx context.Context, _ string) error {
				return c.HTTP.Call(ctx, "POST", base(worker.URL)+"/v1/operations/"+o.ID+"/release", struct{}{}, &nr)
			})
			if e != nil {
				return e
			}
			if e = place.Release(ctx, o.Result.Placement); e != nil {
				return e
			}
			o.State = "Released"
		} else if nr.State == "Ready" {
			if o.State != "Ready" {
				e = cdr.Commit(ctx, o.Result)
				if e != nil {
					return e
				}
			}
			o.State = "Ready"
		} else {
			o.State = nr.State
		}
		o.Reason = nr.Reason
		return c.Store.Save(ctx, o)
	})
}
func (c *Control) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	p, ok := c.Config.Principals[identity(r)]
	if !ok {
		problem(w, 403, errors.New("unmapped client identity"))
		return
	}
	if strings.HasPrefix(r.URL.Path, "/v1/volumes") {
		c.volumeHTTP(w, r, p)
		return
	}
	if r.URL.Path == "/v1/sandboxes" && r.Method == "POST" {
		var q CreateRequest
		if e := decode(w, r, &q); e != nil {
			problem(w, 400, e)
			return
		}
		if e := q.Validate(p); e != nil {
			problem(w, 400, e)
			return
		}
		for _, m := range q.Mounts {
			pvc, err := c.Kube.API.CoreV1().PersistentVolumeClaims(m.Namespace).Get(r.Context(), m.PVC, metav1.GetOptions{})
			if err != nil {
				problem(w, 409, err)
				return
			}
			if pvc.Spec.VolumeMode == nil || *pvc.Spec.VolumeMode != v1.PersistentVolumeBlock {
				problem(w, 400, ErrUnsupported)
				return
			}
		}
		res, e := c.ResolveLaunch(&q)
		if e != nil {
			problem(w, 400, e)
			return
		}
		o, e := c.Store.Create(r.Context(), p, q, res)
		if e != nil {
			problem(w, 409, e)
			return
		}
		c.Kick(o.ID)
		respond(w, 202, o)
		return
	}
	parts := strings.Split(strings.Trim(r.URL.Path, "/"), "/")
	if len(parts) > 4 && parts[0] == "v1" && parts[1] == "sandboxes" && parts[3] == "guest" {
		c.guestHTTP(w, r, p, parts)
		return
	}
	if len(parts) == 3 && parts[0] == "v1" && (parts[1] == "operations" || parts[1] == "sandboxes") {
		o, e := c.Store.Get(r.Context(), parts[2])
		if e != nil || o.Tenant != p.Tenant {
			problem(w, 404, errors.New("operation not found"))
			return
		}
		switch r.Method {
		case "GET":
			respond(w, 200, o)
			return
		case "DELETE":
			if e = c.Store.Stop(r.Context(), o.ID, p.Tenant); e != nil {
				problem(w, 500, e)
				return
			}
			// Push cancellation independently of the creation worker's lock.
			// A tombstone also prevents a delayed Start arriving after this RPC.
			for _, worker := range c.Config.Workers {
				if worker.Name == o.Result.Placement.Node {
					ctx, cancel := context.WithTimeout(r.Context(), 3*time.Second)
					_ = c.HTTP.Call(ctx, "POST", base(worker.URL)+"/v1/revoke", map[string]string{"id": o.ID, "instance": o.Instance}, nil)
					cancel()
				}
			}
			c.Kick(o.ID)
			o.Stop = true
			respond(w, 202, o)
			return
		}
	}
	problem(w, 404, fmt.Errorf("unsupported adapter API: %s %s", r.Method, r.URL.Path))
}
