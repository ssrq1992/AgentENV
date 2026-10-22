// Package storage defines the optional native storage plugin protocol. It has
// no Kubernetes or CSI dependency and is also used by the plugin-disabled build.
package storage

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"
)

const Version = "agentenv.storage.v2"
const ContextHeader = "X-Agentenv-Storage-Context"
const MaxEnvelopeBytes = 1 << 20
const MaxBodyBytes = 512 << 10

type Envelope struct {
	Version   string          `json:"version"`
	Role      string          `json:"role"`
	RequestID string          `json:"request_id"`
	IssuedAt  int64           `json:"issued_at"`
	Payload   json.RawMessage `json:"payload"`
	Signature string          `json:"signature"`
}

type LaunchIntent struct {
	Method     string `json:"method"`
	Path       string `json:"path"`
	Body       []byte `json:"body"`
	BodyDigest string `json:"body_digest"`
	// A native node may request admission on its existing placement for resume,
	// template builds, or Fork; a public caller cannot choose this field.
	NodeID  string `json:"node_id,omitempty"`
	OwnerID string `json:"owner_id,omitempty"`
}

type Command struct {
	Action  string          `json:"action"`
	NodeID  string          `json:"node_id"`
	OwnerID string          `json:"owner_id,omitempty"`
	RunID   string          `json:"run_id,omitempty"`
	Data    json.RawMessage `json:"data"`
}

func Digest(b []byte) string { h := sha256.Sum256(b); return hex.EncodeToString(h[:]) }
func NewRequestID() (string, error) {
	b := make([]byte, 24)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return hex.EncodeToString(b), nil
}
func LoadKey(path string) ([]byte, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	b = []byte(strings.TrimSpace(string(b)))
	if len(b) < 32 {
		return nil, errors.New("storage authentication key must contain at least 32 bytes")
	}
	return b, nil
}
func Sign(key []byte, role, id string, payload any, now time.Time) ([]byte, error) {
	if len(key) < 32 || role == "" || id == "" {
		return nil, errors.New("storage signing identity required")
	}
	p, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	e := Envelope{Version: Version, Role: role, RequestID: id, IssuedAt: now.Unix(), Payload: p}
	raw, err := json.Marshal(e)
	if err != nil {
		return nil, err
	}
	m := hmac.New(sha256.New, key)
	_, _ = m.Write(raw)
	e.Signature = base64.RawURLEncoding.EncodeToString(m.Sum(nil))
	raw, err = json.Marshal(e)
	if len(raw) > MaxEnvelopeBytes {
		return nil, errors.New("storage envelope exceeds limit")
	}
	return raw, err
}
func Verify(key, raw []byte, role string, now time.Time) (Envelope, error) {
	var e Envelope
	if len(key) < 32 || len(raw) == 0 || len(raw) > MaxEnvelopeBytes {
		return e, errors.New("invalid storage envelope size or key")
	}
	if err := json.Unmarshal(raw, &e); err != nil {
		return e, err
	}
	if e.Version != Version || e.Role != role || e.RequestID == "" {
		return e, errors.New("storage protocol or role mismatch")
	}
	age := now.Sub(time.Unix(e.IssuedAt, 0))
	if age < -30*time.Second || age > 5*time.Minute {
		return e, errors.New("storage envelope expired")
	}
	sig, err := base64.RawURLEncoding.DecodeString(e.Signature)
	if err != nil {
		return e, err
	}
	unsigned := e
	unsigned.Signature = ""
	b, err := json.Marshal(unsigned)
	if err != nil {
		return e, err
	}
	m := hmac.New(sha256.New, key)
	_, _ = m.Write(b)
	if !hmac.Equal(sig, m.Sum(nil)) {
		return e, errors.New("storage signature mismatch")
	}
	return e, nil
}
func (i LaunchIntent) Validate() error {
	if i.Method != "POST" || len(i.Body) == 0 || len(i.Body) > MaxBodyBytes || Digest(i.Body) != i.BodyDigest {
		return errors.New("invalid native launch body")
	}
	switch i.Path {
	case "/sandboxes-cold", "/sandboxes", "/v2/sandboxes":
	default:
		return fmt.Errorf("unsupported launch path %q", i.Path)
	}
	var body map[string]json.RawMessage
	if err := json.Unmarshal(i.Body, &body); err != nil || body == nil {
		return errors.New("native launch body must be an object")
	}
	return nil
}
