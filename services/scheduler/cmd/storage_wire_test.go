//go:build ssf

package main

import (
	"bytes"
	"testing"
	"time"

	schedulerv1 "agentenv/services/api/proto"
	"agentenv/services/shared/storage"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/types/known/wrapperspb"
	"ssf.dev/ssf/auth"
)

// The adapter deliberately does not import AgentENV's generated protocol. Both
// directions must retain the opaque, signed bytes across its field-1 bridge.
func TestStorageControlWireCompatibility(t *testing.T) {
	payload := []byte(`{"signed":"opaque bytes"}`)
	raw, err := proto.Marshal(wrapperspb.Bytes(payload))
	if err != nil {
		t.Fatal(err)
	}
	var request schedulerv1.StorageControlRequest
	if err := proto.Unmarshal(raw, &request); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(request.Envelope, payload) {
		t.Fatal("request envelope changed")
	}
	raw, err = proto.Marshal(&schedulerv1.StorageControlResponse{Envelope: payload})
	if err != nil {
		t.Fatal(err)
	}
	var response wrapperspb.BytesValue
	if err := proto.Unmarshal(raw, &response); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(response.Value, payload) {
		t.Fatal("response envelope changed")
	}
}

func TestStorageEnvelopeInteroperability(t *testing.T) {
	key := bytes.Repeat([]byte("k"), 32)
	now := time.Unix(1800000000, 0)
	payload := map[string]any{"request_id": "run-1", "path": "/data/<test>", "body": []byte(" {\n \"volumeMounts\": [] } ")}
	native, err := storage.Sign(key, "node", "rpc-1", payload, now)
	if err != nil {
		t.Fatal(err)
	}
	common, err := auth.SignJSONEnvelope(key, storage.Version, "node", "rpc-1", payload, now)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(native, common) {
		t.Fatal("native and SSF signing formats differ")
	}
	if _, err := auth.VerifyJSONEnvelope(key, native, storage.Version, "node", now); err != nil {
		t.Fatal(err)
	}
	if _, err := storage.Verify(key, common, "node", now); err != nil {
		t.Fatal(err)
	}
	if _, err := storage.Verify(key, common, "scheduler", now); err == nil {
		t.Fatal("node envelope accepted as scheduler")
	}
}
