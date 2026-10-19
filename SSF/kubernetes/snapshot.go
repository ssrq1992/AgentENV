package kubernetes

import (
	"context"
	"errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
)

var snapshots = schema.GroupVersionResource{Group: "snapshot.storage.k8s.io", Version: "v1", Resource: "volumesnapshots"}

type SnapshotState struct {
	UID, Content, Reason string
	Ready                bool
}

func (c *Client) SnapshotStatus(ctx context.Context, namespace, name string) (SnapshotState, error) {
	var s SnapshotState
	if c.Dynamic == nil {
		return s, errors.New("snapshot client unavailable")
	}
	v, e := c.Dynamic.Resource(snapshots).Namespace(namespace).Get(ctx, name, metav1.GetOptions{})
	if e != nil {
		return s, e
	}
	s.UID = string(v.GetUID())
	s.Ready, _, _ = unstructured.NestedBool(v.Object, "status", "readyToUse")
	s.Content, _, _ = unstructured.NestedString(v.Object, "status", "boundVolumeSnapshotContentName")
	s.Reason, _, _ = unstructured.NestedString(v.Object, "status", "error", "message")
	return s, nil
}
func (c *Client) DeleteSnapshot(ctx context.Context, namespace, name, uid string) error {
	if c.Dynamic == nil || uid == "" {
		return errors.New("snapshot client and UID required")
	}
	id := types.UID(uid)
	return c.Dynamic.Resource(snapshots).Namespace(namespace).Delete(ctx, name, metav1.DeleteOptions{Preconditions: &metav1.Preconditions{UID: &id}})
}
