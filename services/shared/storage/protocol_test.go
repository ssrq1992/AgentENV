package storage

import (
	"bytes"
	"encoding/json"
	"testing"
	"time"
)

func TestAuthenticatedIntent(t *testing.T) {
	key := bytes.Repeat([]byte("k"), 32)
	now := time.Now()
	body := []byte(`{ "volumeMounts": [], "image": "original" }`)
	intent := LaunchIntent{Method: "POST", Path: "/sandboxes-cold", Body: body, BodyDigest: Digest(body)}
	signed, err := Sign(key, "gateway", "request", intent, now)
	if err != nil {
		t.Fatal(err)
	}
	e, err := Verify(key, signed, "gateway", now)
	if err != nil {
		t.Fatal(err)
	}
	var decoded LaunchIntent
	if err = json.Unmarshal(e.Payload, &decoded); err != nil {
		t.Fatal(err)
	}
	if err = decoded.Validate(); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(decoded.Body, body) {
		t.Fatal("original body changed")
	}
	if _, err = Verify(key, signed, "node", now); err == nil {
		t.Fatal("role confusion")
	}
	if _, err = Verify(key, signed, "gateway", now.Add(6*time.Minute)); err == nil {
		t.Fatal("expired envelope")
	}
	signed = bytes.Replace(signed, []byte("request"), []byte("changed"), 1)
	if _, err = Verify(key, signed, "gateway", now); err == nil {
		t.Fatal("tampering accepted")
	}
}
