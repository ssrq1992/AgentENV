package agentenvadapter

import (
	"encoding/json"
	"errors"
	v1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"net/http"
	"ssf.dev/ssf/core"
	"strings"
)

func (c *Control) volumeHTTP(w http.ResponseWriter, r *http.Request, p Principal) {
	ctx := r.Context()
	if r.URL.Path == "/v1/volumes" && r.Method == "POST" {
		var q VolumeRequest
		if e := decode(w, r, &q); e != nil {
			problem(w, 400, e)
			return
		}
		if !p.Allows(q.Namespace) || !safeID.MatchString(q.Name) || q.Bytes <= 0 || q.StorageClass == "" {
			problem(w, 400, errors.New("invalid volume request"))
			return
		}
		raw, _ := json.Marshal(q)
		_, e := c.Store.Pool.Exec(ctx, `INSERT INTO agentenv_adapter_volumes(namespace,name,tenant,digest,request,initialize_ext4) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING`, q.Namespace, q.Name, p.Tenant, core.Digest(q), raw, q.InitializeExt4)
		if e != nil {
			problem(w, 500, e)
			return
		}
		var digest, tenant, uid string
		e = c.Store.Pool.QueryRow(ctx, `SELECT digest,tenant,uid FROM agentenv_adapter_volumes WHERE namespace=$1 AND name=$2`, q.Namespace, q.Name).Scan(&digest, &tenant, &uid)
		if e != nil || digest != core.Digest(q) || tenant != p.Tenant {
			problem(w, 409, core.ErrConflict)
			return
		}
		claim, e := c.Kube.API.CoreV1().PersistentVolumeClaims(q.Namespace).Get(ctx, q.Name, metav1.GetOptions{})
		if apierrors.IsNotFound(e) {
			if uid != "" {
				problem(w, 409, errors.New("original PVC deleted; use a new volume name"))
				return
			}
			mode := v1.PersistentVolumeBlock
			claim, e = c.Kube.CreatePVC(ctx, &v1.PersistentVolumeClaim{ObjectMeta: metav1.ObjectMeta{Name: q.Name, Namespace: q.Namespace, Annotations: map[string]string{"agentenvadapter.ssf.dev/creation": core.Hash(p.Tenant + "/" + digest)}}, Spec: v1.PersistentVolumeClaimSpec{StorageClassName: &q.StorageClass, VolumeMode: &mode, AccessModes: []v1.PersistentVolumeAccessMode{v1.ReadWriteOnce}, Resources: v1.VolumeResourceRequirements{Requests: v1.ResourceList{v1.ResourceStorage: *resource.NewQuantity(q.Bytes, resource.BinarySI)}}}})
		}
		if e != nil {
			problem(w, 409, e)
			return
		}
		if claim.Annotations["agentenvadapter.ssf.dev/creation"] != core.Hash(p.Tenant+"/"+digest) || (uid != "" && uid != string(claim.UID)) {
			problem(w, 409, errors.New("existing PVC is not this creation intent"))
			return
		}
		_, e = c.Store.Pool.Exec(ctx, `UPDATE agentenv_adapter_volumes SET uid=$3 WHERE namespace=$1 AND name=$2 AND (uid='' OR uid=$3)`, q.Namespace, q.Name, string(claim.UID))
		if e != nil {
			problem(w, 500, e)
			return
		}
		respond(w, 200, claim)
		return
	}
	parts := strings.Split(strings.Trim(r.URL.Path, "/"), "/")
	if len(parts) != 4 || !p.Allows(parts[2]) {
		problem(w, 404, errors.New("volume not found"))
		return
	}
	ref := core.PVCRef{Namespace: parts[2], Name: parts[3], UID: r.URL.Query().Get("uid")}
	switch r.Method {
	case "GET":
		v, e := c.Kube.API.CoreV1().PersistentVolumeClaims(ref.Namespace).Get(ctx, ref.Name, metav1.GetOptions{})
		if e != nil {
			problem(w, 404, e)
			return
		}
		respond(w, 200, v)
	case "DELETE":
		if ref.UID == "" {
			problem(w, 400, errors.New("expected PVC uid required"))
			return
		}
		action := r.URL.Query().Get("reclaim")
		if action == "" {
			action = "Retain"
		}
		if action != "Retain" && action != "Delete" {
			problem(w, 400, errors.New("reclaim must be Retain or Delete"))
			return
		}
		// Ownership and expected UID are checked again by the shared coordinator.
		cd, _, e := c.coordinator(Operation{Tenant: p.Tenant})
		if e != nil {
			problem(w, 403, e)
			return
		}
		state, e := cd.DeletePVC(ctx, ref, action)
		if e != nil {
			problem(w, 409, e)
			return
		}
		respond(w, 202, map[string]string{"state": state})
	default:
		problem(w, 405, errors.New("method not allowed"))
	}
}
