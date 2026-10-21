package gateway

import (
	schedulerv1 "agentenv/services/api/proto"
	"agentenv/services/shared/storage"
	"bytes"
	"context"
	"encoding/json"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestStorageIntentPreservesBodyAndMountPresence(t *testing.T) {
	key := bytes.Repeat([]byte("k"), 32)
	s := &Server{storageKey: key, apiKey: []byte("api")}
	for _, body := range []string{`{ "templateID": "alias" }`, `{ "templateID": "alias", "volumeMounts": [] }`, `{ "templateID": "alias", "volumeMounts": [{"name":"data","path":"/data"}] }`} {
		req := httptest.NewRequest("POST", "/sandboxes", strings.NewReader(body))
		req.Header.Set(headerAPIKey, "api")
		signed, err := s.storageIntent(req)
		if err != nil {
			t.Fatal(err)
		}
		envelope, err := storage.Verify(key, signed, "gateway", time.Now())
		if err != nil {
			t.Fatal(err)
		}
		var intent storage.LaunchIntent
		if err = json.Unmarshal(envelope.Payload, &intent); err != nil {
			t.Fatal(err)
		}
		actual, err := io.ReadAll(req.Body)
		if err != nil {
			t.Fatal(err)
		}
		if string(actual) != body || string(intent.Body) != body || intent.BodyDigest != storage.Digest(actual) {
			t.Fatal("native request body changed")
		}
	}
}
func TestStorageIntentCannotDowngradeMalformedOrOversizedLaunch(t *testing.T) {
	s := &Server{storageKey: bytes.Repeat([]byte("k"), 32), apiKey: []byte("api")}
	for _, body := range []string{`{broken`, strings.Repeat(" ", storage.MaxBodyBytes+1)} {
		req := httptest.NewRequest("POST", "/sandboxes-cold", strings.NewReader(body))
		req.Header.Set(headerAPIKey, "api")
		if _, err := s.storageIntent(req); err == nil {
			t.Fatal("invalid body silently bypassed storage")
		}
	}
	req := httptest.NewRequest("POST", "/sandboxes-cold", strings.NewReader(`{}`))
	if _, err := s.storageIntent(req); err == nil {
		t.Fatal("unauthenticated storage admission")
	}
	req.Header.Set(storage.ContextHeader, "forged")
	req.Header.Set("X-Agentenv-Storage-Owner", "victim")
	req.Header.Set("X-Custom-Extension", "keep")
	stripStorageHeaders(req.Header)
	if req.Header.Get(storage.ContextHeader) != "" || req.Header.Get("X-Agentenv-Storage-Owner") != "" || req.Header.Get("X-Custom-Extension") != "keep" {
		t.Fatal("incorrect private header filtering")
	}
}

func TestNativeStorageConflictPreservesOrdinarySchedulerErrorMapping(t *testing.T) {
	for _, enabled := range []bool{false, true} {
		client := stubSchedulerClient{scheduleFunc: func(_ context.Context, q *schedulerv1.ScheduleRequest, _ ...grpc.CallOption) (*schedulerv1.ScheduleResponse, error) {
			if (len(q.StorageIntent) > 0) != enabled {
				t.Fatal("incorrect storage intent")
			}
			return nil, status.Error(codes.FailedPrecondition, "writer already in use")
		}}
		server := newTestServer(t, client, time.Second, 1024)
		if enabled {
			server.storageKey = bytes.Repeat([]byte("k"), 32)
		}
		req := httptest.NewRequest(http.MethodPost, "/sandboxes-cold", strings.NewReader(`{"image":"test"}`))
		req.Header.Set(headerAPIKey, testAPIKey)
		response := httptest.NewRecorder()
		server.Handler().ServeHTTP(response, req)
		expected := http.StatusBadGateway
		if enabled {
			expected = http.StatusConflict
		}
		if response.Code != expected {
			t.Fatalf("storage=%t: %d %s", enabled, response.Code, response.Body.String())
		}
	}
}
