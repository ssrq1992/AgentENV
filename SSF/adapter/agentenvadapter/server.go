package agentenvadapter

import (
	"context"
	"crypto/ed25519"
	"encoding/base64"
	"errors"
	"fmt"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
	"net"
	"net/http"
	"os"
	v1 "ssf.dev/ssf/api/ssf/v1"
	"ssf.dev/ssf/auth"
	"ssf.dev/ssf/config"
	kube "ssf.dev/ssf/kubernetes"
	"ssf.dev/ssf/ledger"
	"ssf.dev/ssf/observe"
	"strings"
	"time"
)

type storageClient struct{ Client v1.RuntimeStorageNodeClient }

func (s storageClient) PrepareVolumes(c context.Context, q *v1.PrepareRequest) (*v1.PrepareResponse, error) {
	return s.Client.PrepareVolumes(c, q)
}
func (s storageClient) Ready(c context.Context, op, instance, proof string) error {
	_, e := s.Client.MarkRuntimeReady(c, &v1.RuntimeRequest{OperationId: op, SandboxInstanceUid: instance, Evidence: proof})
	return e
}
func (s storageClient) Release(c context.Context, op, instance, proof, plan string) error {
	_, e := s.Client.ReleaseVolumes(c, &v1.RuntimeRequest{OperationId: op, SandboxInstanceUid: instance, Evidence: proof, SignedNodePlan: plan})
	return e
}
func readKey(file string, size int) ([]byte, error) {
	b, e := os.ReadFile(file)
	if e != nil {
		return nil, e
	}
	v, e := base64.StdEncoding.DecodeString(strings.TrimSpace(string(b)))
	if e != nil {
		return nil, e
	}
	if len(v) != size {
		return nil, errors.New("invalid signing key size")
	}
	return v, nil
}
func Run(ctx context.Context, role string, c Config) error {
	if e := c.Validate(role); e != nil {
		return e
	}
	serverTLS, e := auth.TLS(c.Cert, c.Key, c.CA, true)
	if e != nil {
		return e
	}
	clientTLS, e := auth.TLS(c.Cert, c.Key, c.CA, false)
	if e != nil {
		return e
	}
	client := &HTTPClient{Client: &http.Client{Transport: &http.Transport{TLSClientConfig: clientTLS, MaxIdleConns: 128, MaxIdleConnsPerHost: 32}, CheckRedirect: func(*http.Request, []*http.Request) error { return errors.New("redirect forbidden") }}}
	rest, e := config.Kube(c.Kubeconfig)
	if e != nil {
		return e
	}
	api, e := kubernetes.NewForConfig(rest)
	if e != nil {
		return e
	}
	metrics := &observe.Registry{}
	var handler http.Handler
	var readiness func(context.Context) error
	if role == "control" {
		db, e := ledger.Open(ctx, c.DatabaseURL)
		if e != nil {
			return e
		}
		defer db.Close()
		key, e := readKey(c.SigningKey, ed25519.PrivateKeySize)
		if e != nil {
			return e
		}
		store := &Store{db.Pool}
		if _, e = store.Pending(ctx); e != nil {
			return fmt.Errorf("run ctl migrate first: %w", e)
		}
		registry := &config.Registry{Config: config.Config{Drivers: c.Drivers}, API: api, TLS: clientTLS}
		control := &Control{Config: c, Store: store, Ledger: db, Kube: &kube.Client{API: api}, Drivers: registry, Key: ed25519.PrivateKey(key), HTTP: client, Metrics: metrics, Context: ctx}
		handler = control
		readiness = db.Pool.Ping
		go control.Recover(ctx)
	} else if role == "node" {
		host, _, e := net.SplitHostPort(c.HookListen)
		if e != nil || (host != "127.0.0.1" && host != "::1") {
			return errors.New("hook listener must be Pod loopback")
		}
		k, e := api.CoreV1().Nodes().Get(ctx, c.NodeName, metav1.GetOptions{})
		if e != nil {
			return e
		}
		if c.NodeUID != "" && c.NodeUID != string(k.UID) {
			return errors.New("configured NodeUID differs")
		}
		c.NodeUID = string(k.UID)
		boot, e := os.ReadFile("/proc/sys/kernel/random/boot_id")
		if e != nil {
			return e
		}
		pub, e := readKey(c.PlanPublicKey, ed25519.PublicKeySize)
		if e != nil {
			return e
		}
		key, e := os.ReadFile(c.AgentKeyFile)
		if e != nil {
			return e
		}
		j, e := OpenJournal(c.Root)
		if e != nil {
			return e
		}
		defer j.Close()
		conn, e := grpc.NewClient(c.SSFEndpoint, grpc.WithTransportCredentials(credentials.NewTLS(clientTLS)))
		if e != nil {
			return e
		}
		defer conn.Close()
		agent := &HTTPClient{Client: &http.Client{Transport: &http.Transport{MaxIdleConnsPerHost: 32}, CheckRedirect: func(*http.Request, []*http.Request) error { return errors.New("redirect forbidden") }}, APIKey: strings.TrimSpace(string(key))}
		pidns, e := os.Readlink("/proc/self/ns/pid")
		if e != nil {
			return e
		}
		n := &Node{PIDNamespace: pidns, Config: c, BootID: strings.TrimSpace(string(boot)), Key: ed25519.PublicKey(pub), Journal: j, SSF: storageClient{v1.NewRuntimeStorageNodeClient(conn)}, Agent: agent, Context: ctx, Metrics: metrics}
		handler = n
		readiness = func(ctx context.Context) error {
			return agent.Call(ctx, "GET", base(c.AgentURL)+"/health", nil, nil)
		}
		hook := httpServer(c.HookListen, http.HandlerFunc(n.Hooks))
		defer hook.Close()
		go func() {
			if e := hook.ListenAndServe(); e != nil && !errors.Is(e, http.ErrServerClosed) {
				fmt.Fprintln(os.Stderr, e)
			}
		}()
		go n.Recover(ctx)
	} else {
		return errors.New("role must be control or node")
	}
	if c.HealthListen != "" {
		mux := http.NewServeMux()
		mux.Handle("/metrics", metrics)
		mux.HandleFunc("/livez", func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(200) })
		mux.HandleFunc("/readyz", func(w http.ResponseWriter, r *http.Request) {
			ctx, cancel := context.WithTimeout(r.Context(), 2*time.Second)
			defer cancel()
			if e := readiness(ctx); e != nil {
				problem(w, 503, e)
				return
			}
			w.WriteHeader(200)
		})
		h := httpServer(c.HealthListen, mux)
		defer h.Close()
		go h.ListenAndServe()
	}
	h := httpServer(c.Listen, handler)
	h.TLSConfig = serverTLS
	go func() { <-ctx.Done(); _ = h.Close() }()
	e = h.ListenAndServeTLS("", "")
	if errors.Is(e, http.ErrServerClosed) {
		return nil
	}
	return e
}
func Migrate(ctx context.Context, c Config) error {
	db, e := ledger.Open(ctx, c.DatabaseURL)
	if e != nil {
		return e
	}
	defer db.Close()
	if e = db.Migrate(ctx); e != nil {
		return e
	}
	return (&Store{db.Pool}).Migrate(ctx)
}
