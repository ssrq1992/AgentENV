package agentenvadapter

import (
	"context"
	"errors"
	"github.com/jackc/pgx/v5"
	v1 "k8s.io/api/core/v1"
	storagev1 "k8s.io/api/storage/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/labels"
	"ssf.dev/ssf/core"
	kube "ssf.dev/ssf/kubernetes"
	"time"
)

type NodeInfo struct {
	Name   string `json:"name"`
	UID    string `json:"uid"`
	BootID string `json:"boot_id"`
	Ready  bool   `json:"ready"`
}
type Placement struct {
	Store     *Store
	Workers   []Worker
	HTTP      *HTTPClient
	Kube      *kube.Client
	Resources Resources
}

func (p *Placement) node(ctx context.Context, name string) (Worker, NodeInfo, error) {
	for _, n := range p.Workers {
		if n.Name == name {
			var info NodeInfo
			e := p.HTTP.Call(ctx, "GET", base(n.URL)+"/v1/info", nil, &info)
			if e != nil {
				return n, info, e
			}
			k, e := p.Kube.API.CoreV1().Nodes().Get(ctx, name, metav1.GetOptions{})
			if e != nil {
				return n, info, e
			}
			if !info.Ready || info.Name != name || info.UID != string(k.UID) || info.BootID == "" || k.Spec.Unschedulable || k.DeletionTimestamp != nil {
				return n, info, errors.New("worker not ready or identity mismatch")
			}
			return n, info, nil
		}
	}
	return Worker{}, NodeInfo{}, errors.New("worker not configured")
}
func (p *Placement) Reserve(ctx context.Context, op string, volumes []core.Volume) (core.Placement, error) {
	var prior core.Placement
	e := p.Store.Pool.QueryRow(ctx, `SELECT operation,node,node_uid,boot_id,expires FROM agentenv_adapter_reservations WHERE operation=$1 AND state!='Released'`, op).Scan(&prior.ID, &prior.Node, &prior.NodeUID, &prior.BootID, &prior.Expires)
	if e == nil {
		return prior, p.Validate(ctx, prior)
	}
	if !errors.Is(e, pgx.ErrNoRows) {
		return prior, e
	}
	for _, candidate := range p.Workers {
		n, info, e := p.node(ctx, candidate.Name)
		if e != nil {
			continue
		}
		valid := true
		for _, v := range volumes {
			if v.Handle != "" {
				if e = p.Kube.CheckTopology(ctx, v, n.Name); e != nil {
					valid = false
					break
				}
			} else if e = p.pendingTopology(ctx, v.PVC, n.Name); e != nil {
				valid = false
				break
			}
		}
		if !valid {
			continue
		}
		tx, e := p.Store.Pool.Begin(ctx)
		if e != nil {
			return prior, e
		}
		_, e = tx.Exec(ctx, `SELECT pg_advisory_xact_lock(hashtextextended($1,38))`, n.Name)
		var cpu, mem int64
		if e == nil {
			e = tx.QueryRow(ctx, `SELECT COALESCE(sum(cpu),0),COALESCE(sum(memory_mib),0) FROM agentenv_adapter_reservations WHERE node=$1 AND state!='Released'`, n.Name).Scan(&cpu, &mem)
		}
		if e != nil {
			tx.Rollback(ctx)
			return prior, e
		}
		if cpu+p.Resources.CPU > n.Resources.CPU || mem+p.Resources.MemoryMiB > n.Resources.MemoryMiB {
			tx.Rollback(ctx)
			continue
		}
		out := core.Placement{ID: op, Node: n.Name, NodeUID: info.UID, BootID: info.BootID, Expires: time.Now().Add(5 * time.Minute)}
		_, e = tx.Exec(ctx, `INSERT INTO agentenv_adapter_reservations(operation,node,node_uid,boot_id,cpu,memory_mib,expires) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING`, op, n.Name, info.UID, info.BootID, p.Resources.CPU, p.Resources.MemoryMiB, out.Expires)
		if e == nil {
			e = tx.Commit(ctx)
		} else {
			tx.Rollback(ctx)
		}
		if e != nil {
			return prior, e
		}
		// Another replica may have inserted the same operation first.
		e = p.Store.Pool.QueryRow(ctx, `SELECT operation,node,node_uid,boot_id,expires FROM agentenv_adapter_reservations WHERE operation=$1`, op).Scan(&out.ID, &out.Node, &out.NodeUID, &out.BootID, &out.Expires)
		return out, e
	}
	return prior, errors.New("no compatible dedicated capacity; reservation retained on unknown outcomes")
}
func (p *Placement) pendingTopology(ctx context.Context, ref core.PVCRef, node string) error {
	claim, e := p.Kube.API.CoreV1().PersistentVolumeClaims(ref.Namespace).Get(ctx, ref.Name, metav1.GetOptions{})
	if e != nil {
		return e
	}
	if claim.Spec.StorageClassName == nil {
		return errors.New("storage class required")
	}
	if claim.Spec.VolumeName != "" {
		pv, e := p.Kube.API.CoreV1().PersistentVolumes().Get(ctx, claim.Spec.VolumeName, metav1.GetOptions{})
		if e != nil {
			return e
		}
		return p.Kube.CheckTopology(ctx, core.Volume{PVName: pv.Name, PVUID: string(pv.UID)}, node)
	}
	sc, e := p.Kube.API.StorageV1().StorageClasses().Get(ctx, *claim.Spec.StorageClassName, metav1.GetOptions{})
	if e != nil {
		return e
	}
	// Immediate provisioning has no scheduler-selected topology. Wait for its
	// actual PV before reserving compute rather than guessing a future zone.
	if sc.VolumeBindingMode == nil || *sc.VolumeBindingMode == storagev1.VolumeBindingImmediate {
		return core.ErrPending
	}
	if sc.Provisioner == "kubernetes.io/no-provisioner" || claim.Spec.Selector != nil {
		list, e := p.Kube.API.CoreV1().PersistentVolumes().List(ctx, metav1.ListOptions{LabelSelector: kube.ManagedLabel + "=true"})
		if e != nil {
			return e
		}
		found := false
		for _, pv := range list.Items {
			if pv.Spec.CSI == nil || pv.Spec.StorageClassName != sc.Name || pv.Spec.PersistentVolumeReclaimPolicy != v1.PersistentVolumeReclaimRetain || pv.DeletionTimestamp != nil {
				continue
			}
			if pv.Spec.ClaimRef != nil && pv.Spec.ClaimRef.UID != claim.UID {
				continue
			}
			if pv.Spec.ClaimRef == nil && pv.Status.Phase != v1.VolumeAvailable {
				continue
			}
			if claim.Spec.Selector != nil {
				sel, e := metav1.LabelSelectorAsSelector(claim.Spec.Selector)
				if e != nil {
					return e
				}
				if !sel.Matches(labels.Set(pv.Labels)) {
					continue
				}
			}
			want, have := claim.Spec.Resources.Requests[v1.ResourceStorage], pv.Spec.Capacity[v1.ResourceStorage]
			if have.Cmp(want) < 0 || pv.Spec.VolumeMode == nil || *pv.Spec.VolumeMode != v1.PersistentVolumeBlock {
				continue
			}
			compatible := true
			for _, a := range claim.Spec.AccessModes {
				ok := false
				for _, b := range pv.Spec.AccessModes {
					ok = ok || a == b
				}
				compatible = compatible && ok
			}
			if !compatible {
				continue
			}
			if e = p.Kube.CheckTopology(ctx, core.Volume{PVName: pv.Name, PVUID: string(pv.UID)}, node); e == nil {
				found = true
				break
			} else if !errors.Is(e, kube.ErrTopology) {
				return e
			}
		}
		if !found {
			return errors.New("no static PV compatible with node and claim")
		}
	}
	if len(sc.AllowedTopologies) == 0 {
		return nil
	}
	n, e := p.Kube.API.CoreV1().Nodes().Get(ctx, node, metav1.GetOptions{})
	if e != nil {
		return e
	}
	for _, term := range sc.AllowedTopologies {
		ok := true
		for _, expr := range term.MatchLabelExpressions {
			found := false
			for _, v := range expr.Values {
				if n.Labels[expr.Key] == v {
					found = true
				}
			}
			ok = ok && found
		}
		if ok {
			return nil
		}
	}
	return errors.New("StorageClass topology excludes node")
}
func (p *Placement) Validate(ctx context.Context, r core.Placement) error {
	_, n, e := p.node(ctx, r.Node)
	if e != nil {
		return e
	}
	if n.UID != r.NodeUID || n.BootID != r.BootID {
		return errors.New("reserved node incarnation changed")
	}
	var state string
	e = p.Store.Pool.QueryRow(ctx, `SELECT state FROM agentenv_adapter_reservations WHERE operation=$1 AND node=$2 AND node_uid=$3 AND boot_id=$4`, r.ID, r.Node, r.NodeUID, r.BootID).Scan(&state)
	if e == nil && state == "Released" {
		return core.ErrConflict
	}
	return e
}
func (p *Placement) Renew(ctx context.Context, r core.Placement) (core.Placement, error) {
	if e := p.Validate(ctx, r); e != nil {
		return r, e
	}
	r.Expires = time.Now().Add(5 * time.Minute)
	_, e := p.Store.Pool.Exec(ctx, `UPDATE agentenv_adapter_reservations SET expires=$2 WHERE operation=$1 AND state!='Released'`, r.ID, r.Expires)
	return r, e
}
func (p *Placement) Commit(ctx context.Context, r core.Placement, _ string) error {
	if e := p.Validate(ctx, r); e != nil {
		return e
	}
	_, e := p.Store.Pool.Exec(ctx, `UPDATE agentenv_adapter_reservations SET state='Committed' WHERE operation=$1 AND state!='Released'`, r.ID)
	return e
}
func (p *Placement) Release(ctx context.Context, r core.Placement) error {
	_, e := p.Store.Pool.Exec(ctx, `UPDATE agentenv_adapter_reservations SET state='Released' WHERE operation=$1`, r.ID)
	return e
}
