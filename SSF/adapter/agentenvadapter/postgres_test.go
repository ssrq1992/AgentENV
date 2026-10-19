package agentenvadapter

import (
	"context"
	"fmt"
	"github.com/jackc/pgx/v5/pgxpool"
	v1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes/fake"
	"net/http"
	"net/http/httptest"
	"os"
	kube "ssf.dev/ssf/kubernetes"
	"sync"
	"testing"
)

func postgresStore(t *testing.T) *Store {
	t.Helper()
	url := os.Getenv("SSF_ADAPTER_TEST_DATABASE_URL")
	if url == "" {
		t.Skip("set SSF_ADAPTER_TEST_DATABASE_URL to a disposable PostgreSQL database")
	}
	ctx := context.Background()
	admin, e := pgxpool.New(ctx, url)
	if e != nil {
		t.Fatal(e)
	}
	schema := "aenv_test_" + newID()
	if _, e = admin.Exec(ctx, "CREATE SCHEMA "+schema); e != nil {
		t.Fatal(e)
	}
	cfg, e := pgxpool.ParseConfig(url)
	if e != nil {
		t.Fatal(e)
	}
	cfg.ConnConfig.RuntimeParams["search_path"] = schema
	pool, e := pgxpool.NewWithConfig(ctx, cfg)
	if e != nil {
		t.Fatal(e)
	}
	t.Cleanup(func() { pool.Close(); _, _ = admin.Exec(ctx, "DROP SCHEMA "+schema+" CASCADE"); admin.Close() })
	s := &Store{pool}
	if e = s.Migrate(ctx); e != nil {
		t.Fatal(e)
	}
	return s
}
func TestPostgresConcurrentIdempotency(t *testing.T) {
	s := postgresStore(t)
	ctx := context.Background()
	p := Principal{Tenant: "a"}
	q := validRequest()
	var wg sync.WaitGroup
	ids := make(chan string, 20)
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			o, e := s.Create(ctx, p, q, Resources{2, 512})
			if e != nil {
				t.Error(e)
				return
			}
			ids <- o.Instance
		}()
	}
	wg.Wait()
	close(ids)
	first := ""
	for id := range ids {
		if first != "" && first != id {
			t.Fatal("duplicate instances")
		}
		first = id
	}
	q.Kind = "template"
	if _, e := s.Create(ctx, p, q, Resources{2, 512}); e == nil {
		t.Fatal("idempotency conflict accepted")
	}
}
func TestPostgresStopCannotBeOverwritten(t *testing.T) {
	s := postgresStore(t)
	ctx := context.Background()
	o, e := s.Create(ctx, Principal{Tenant: "a"}, validRequest(), Resources{2, 512})
	if e != nil {
		t.Fatal(e)
	}
	if e = s.Stop(ctx, o.ID, "a"); e != nil {
		t.Fatal(e)
	}
	o.State = "Preparing"
	if e = s.Save(ctx, o); e != nil {
		t.Fatal(e)
	}
	o, e = s.Get(ctx, o.ID)
	if e != nil || !o.Stop {
		t.Fatal("lost cancellation", e)
	}
	if e = s.Stop(ctx, o.ID, "other"); e == nil {
		t.Fatal("cross-tenant stop accepted")
	}
}
func TestPostgresInitializationIsSingleOperation(t *testing.T) {
	s := postgresStore(t)
	ctx := context.Background()
	_, e := s.Pool.Exec(ctx, `INSERT INTO agentenv_adapter_volumes(namespace,name,tenant,digest,request,uid,initialize_ext4) VALUES('tenant-a','data','a','digest','{}','uid',true)`)
	if e != nil {
		t.Fatal(e)
	}
	q := validRequest().CoreMounts()[0].PVC
	q.UID = "uid"
	var wg sync.WaitGroup
	var mu sync.Mutex
	winners := 0
	for i := 0; i < 10; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			ok, e := s.AuthorizeInitialization(ctx, q, fmt.Sprint(i))
			if e != nil {
				t.Error(e)
			}
			if ok {
				mu.Lock()
				winners++
				mu.Unlock()
			}
		}(i)
	}
	wg.Wait()
	if winners != 1 {
		t.Fatal("initialization authority has", winners, "owners")
	}
}

func TestPostgresConcurrentPlacementCapacity(t *testing.T) {
	s := postgresStore(t)
	ctx := context.Background()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		respond(w, 200, NodeInfo{Name: "n", UID: "uid", BootID: "boot", Ready: true})
	}))
	defer server.Close()
	p := &Placement{Store: s, Workers: []Worker{{Name: "n", URL: server.URL, Resources: Resources{4, 1024}}}, Resources: Resources{2, 512}, HTTP: &HTTPClient{Client: server.Client()}, Kube: &kube.Client{API: fake.NewClientset(&v1.Node{ObjectMeta: metav1.ObjectMeta{Name: "n", UID: "uid"}})}}
	var wg sync.WaitGroup
	var mu sync.Mutex
	winners := 0
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			_, e := p.Reserve(ctx, fmt.Sprint(i), nil)
			if e == nil {
				mu.Lock()
				winners++
				mu.Unlock()
			}
		}(i)
	}
	wg.Wait()
	if winners != 2 {
		t.Fatalf("capacity admits %d operations, expected 2", winners)
	}
	// Expiry does not free possibly running compute.
	if _, e := s.Pool.Exec(ctx, `UPDATE agentenv_adapter_reservations SET expires=now()-interval '1 hour'`); e != nil {
		t.Fatal(e)
	}
	if _, e := p.Reserve(ctx, "another", nil); e == nil {
		t.Fatal("lease expiration reclaimed a running reservation")
	}
}
