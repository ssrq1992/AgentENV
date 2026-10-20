package auth

import (
	"crypto/ed25519"
	"crypto/tls"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"errors"
	"os"
	"ssf.dev/ssf/core"
	"strings"
	"time"
)

func Sign(p core.Plan, key ed25519.PrivateKey) (string, error) {
	if len(key) != ed25519.PrivateKeySize {
		return "", errors.New("invalid signing key")
	}
	b, e := json.Marshal(p)
	if e != nil {
		return "", e
	}
	return base64.RawURLEncoding.EncodeToString(b) + "." + base64.RawURLEncoding.EncodeToString(ed25519.Sign(key, b)), nil
}
func Verify(token string, key ed25519.PublicKey, node, boot string, now time.Time) (core.Plan, error) {
	p, e := verifySignature(token, key)
	if e != nil {
		return p, e
	}
	if p.Node != node || p.BootID != boot || !now.Before(p.Expires) {
		return p, errors.New("plan scope or expiry mismatch")
	}
	return p, nil
}

// VerifyRevocation authenticates a stop-only plan. Expiry and boot changes must
// not prevent a durable tombstone, but physical node identity must still match.
// Callers must additionally verify actual runtime detachment before persisting it.
func VerifyRevocation(token string, key ed25519.PublicKey, node, nodeUID, boot string) (core.Plan, error) {
	p, err := verifySignature(token, key)
	if err != nil {
		return p, err
	}
	if p.Node != node || (nodeUID != "" && p.NodeUID != nodeUID) || (p.BootID != boot && (nodeUID == "" || p.NodeUID != nodeUID)) {
		return p, errors.New("revocation node identity mismatch")
	}
	return p, nil
}

func verifySignature(token string, key ed25519.PublicKey) (core.Plan, error) {
	var p core.Plan
	s := strings.Split(token, ".")
	if len(s) != 2 || len(key) != ed25519.PublicKeySize {
		return p, errors.New("invalid signed plan")
	}
	b, e := base64.RawURLEncoding.DecodeString(s[0])
	if e != nil {
		return p, e
	}
	sig, e := base64.RawURLEncoding.DecodeString(s[1])
	if e != nil || !ed25519.Verify(key, b, sig) {
		return p, errors.New("invalid signature")
	}
	if e = json.Unmarshal(b, &p); e != nil {
		return p, e
	}
	if p.ID == "" || p.Sandbox == "" || len(p.Bindings) == 0 {
		return p, errors.New("plan scope or expiry mismatch")
	}
	return p, nil
}
func TLS(cert, key, ca string, server bool) (*tls.Config, error) {
	pair, e := tls.LoadX509KeyPair(cert, key)
	if e != nil {
		return nil, e
	}
	b, e := os.ReadFile(ca)
	if e != nil {
		return nil, e
	}
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(b) {
		return nil, errors.New("invalid CA")
	}
	c := &tls.Config{MinVersion: tls.VersionTLS13, Certificates: []tls.Certificate{pair}, RootCAs: pool}
	if server {
		c.ClientCAs = pool
		c.ClientAuth = tls.RequireAndVerifyClientCert
	}
	return c, nil
}
