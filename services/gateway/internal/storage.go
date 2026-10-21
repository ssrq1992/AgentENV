package gateway

import (
	"bytes"
	"crypto/subtle"
	"encoding/base64"
	"errors"
	"io"
	"net/http"
	"strings"
	"time"

	"agentenv/services/shared/storage"
)

func stripStorageHeaders(h http.Header) {
	for name := range h {
		if strings.HasPrefix(strings.ToLower(name), "x-agentenv-storage-") {
			h.Del(name)
		}
	}
}
func (s *Server) storageIntent(r *http.Request) ([]byte, error) {
	if len(s.storageKey) == 0 || r.Method != http.MethodPost {
		return nil, nil
	}
	path := strings.TrimRight(r.URL.Path, "/")
	switch path {
	case "/sandboxes-cold", "/sandboxes", "/v2/sandboxes":
	default:
		return nil, nil
	}
	// Handler authentication precedes this function. This additional assertion
	// prevents accidental reuse from an unauthenticated route in future changes.
	if !s.validStorageAPIKey(r.Header) {
		return nil, errors.New("authenticated launch required")
	}
	original := r.Body
	if original == nil {
		return nil, errors.New("native launch body required")
	}
	body, err := io.ReadAll(io.LimitReader(original, storage.MaxBodyBytes+1))
	_ = original.Close()
	if err != nil {
		return nil, err
	}
	if len(body) > storage.MaxBodyBytes {
		return nil, errors.New("native launch exceeds storage protocol limit")
	}
	r.Body = io.NopCloser(bytes.NewReader(body))
	r.ContentLength = int64(len(body))
	intent := storage.LaunchIntent{Method: r.Method, Path: path, Body: body, BodyDigest: storage.Digest(body)}
	if err = intent.Validate(); err != nil {
		return nil, err
	}
	id, err := storage.NewRequestID()
	if err != nil {
		return nil, err
	}
	return storage.Sign(s.storageKey, "gateway", id, intent, time.Now())
}
func (s *Server) validStorageAPIKey(h http.Header) bool {
	values := h.Values(headerAPIKey)
	return len(values) == 1 && subtle.ConstantTimeCompare([]byte(values[0]), s.apiKey) == 1
}
func encodeStorageDispatch(b []byte) (string, error) {
	if len(b) > storage.MaxEnvelopeBytes {
		return "", errors.New("storage dispatch exceeds native protocol limit")
	}
	return base64.RawURLEncoding.EncodeToString(b), nil
}
