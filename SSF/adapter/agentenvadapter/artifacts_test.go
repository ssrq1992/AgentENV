package agentenvadapter

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	v1 "k8s.io/api/core/v1"
	storage "k8s.io/api/storage/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/kubernetes/fake"
	"k8s.io/client-go/kubernetes/scheme"
	"os"
	"os/exec"
	"reflect"
	"ssf.dev/ssf/core"
	kube "ssf.dev/ssf/kubernetes"
	"testing"
)

func TestRenderedDeploymentTypesAndReproducibility(t *testing.T) {
	if _, e := exec.LookPath("python3"); e != nil {
		t.Skip("python3 required for deployment renderer")
	}
	out, e := exec.Command("python3", "scripts/render.py").Output()
	if e != nil {
		t.Fatal(e)
	}
	prior, e := os.ReadFile("deploy/example.rendered.json")
	if e != nil {
		t.Fatal(e)
	}
	var a, b any
	if e = json.Unmarshal(out, &a); e != nil {
		t.Fatal(e)
	}
	if e = json.Unmarshal(prior, &b); e != nil {
		t.Fatal(e)
	}
	if !reflect.DeepEqual(a, b) {
		t.Fatal("rendered example is stale")
	}
	var list struct {
		Items []json.RawMessage `json:"items"`
	}
	if e = json.Unmarshal(out, &list); e != nil {
		t.Fatal(e)
	}
	for _, raw := range list.Items {
		var m struct {
			APIVersion string `json:"apiVersion"`
			Kind       string `json:"kind"`
		}
		_ = json.Unmarshal(raw, &m)
		gv, e := schema.ParseGroupVersion(m.APIVersion)
		if e != nil {
			t.Fatal(e)
		}
		obj, e := scheme.Scheme.New(gv.WithKind(m.Kind))
		if e != nil {
			t.Fatal(e)
		}
		d := json.NewDecoder(bytes.NewReader(raw))
		d.DisallowUnknownFields()
		if e = d.Decode(obj); e != nil {
			t.Fatalf("%s: %v", m.Kind, e)
		}
	}
}

func TestImmediatePendingDoesNotGuessTopology(t *testing.T) {
	mode := storage.VolumeBindingImmediate
	class := "immediate"
	claim := &v1.PersistentVolumeClaim{ObjectMeta: metav1.ObjectMeta{Namespace: "n", Name: "p", UID: "uid"}, Spec: v1.PersistentVolumeClaimSpec{StorageClassName: &class}}
	p := &Placement{Kube: &kube.Client{API: fake.NewClientset(claim, &storage.StorageClass{ObjectMeta: metav1.ObjectMeta{Name: class}, Provisioner: "example.csi", VolumeBindingMode: &mode})}}
	if e := p.pendingTopology(context.Background(), core.PVCRef{Namespace: "n", Name: "p", UID: "uid"}, "unknown-future-zone"); !errors.Is(e, core.ErrPending) {
		t.Fatal("reserved compute before Immediate PV topology known", e)
	}
}
