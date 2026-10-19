package agentenvadapter

import (
	"context"
	_ "embed"
	"encoding/json"
	"errors"
	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
	"ssf.dev/ssf/core"
	"time"
)

//go:embed schema.sql
var Schema string

type Store struct{ Pool *pgxpool.Pool }

func (s *Store) Migrate(ctx context.Context) error { _, e := s.Pool.Exec(ctx, Schema); return e }
func (s *Store) Create(ctx context.Context, p Principal, q CreateRequest, res Resources) (Operation, error) {
	id := core.Hash(p.Tenant + "\x00" + q.RequestID)
	raw, _ := json.Marshal(q)
	rr, _ := json.Marshal(res)
	_, e := s.Pool.Exec(ctx, `INSERT INTO agentenv_adapter_operations(id,tenant,instance,digest,request,resources) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING`, id, p.Tenant, newID(), core.Digest(q), raw, rr)
	if e != nil {
		return Operation{}, e
	}
	o, e := s.Get(ctx, id)
	if e == nil && core.Digest(o.Request) != core.Digest(q) {
		e = core.ErrConflict
	}
	return o, e
}
func (s *Store) Get(ctx context.Context, id string) (Operation, error) {
	var o Operation
	var q, res, result, native, timings []byte
	e := s.Pool.QueryRow(ctx, `SELECT id,tenant,instance,request,resources,result,state,reason,stopping,sandbox_id,native,updated,timings FROM agentenv_adapter_operations WHERE id=$1`, id).Scan(&o.ID, &o.Tenant, &o.Instance, &q, &res, &result, &o.State, &o.Reason, &o.Stop, &o.SandboxID, &native, &o.Updated, &timings)
	if e != nil {
		return o, e
	}
	for _, pair := range []struct {
		b []byte
		v any
	}{{q, &o.Request}, {res, &o.Resources}, {result, &o.Result}, {timings, &o.Timings}} {
		if e = json.Unmarshal(pair.b, pair.v); e != nil {
			return o, e
		}
	}
	o.Native = native
	return o, nil
}
func (s *Store) Save(ctx context.Context, o Operation) error {
	r, e := json.Marshal(o.Result)
	if e != nil {
		return e
	}
	timings, _ := json.Marshal(o.Timings)
	_, e = s.Pool.Exec(ctx, `UPDATE agentenv_adapter_operations SET result=$2,state=$3,reason=$4,sandbox_id=$5,native=$6,timings=$7,updated=now() WHERE id=$1`, o.ID, r, o.State, o.Reason, o.SandboxID, nullableJSON(o.Native), timings)
	return e
}
func nullableJSON(b []byte) any {
	if len(b) == 0 {
		return nil
	}
	return b
}
func (s *Store) Stop(ctx context.Context, id, tenant string) error {
	r, e := s.Pool.Exec(ctx, `UPDATE agentenv_adapter_operations SET stopping=true,updated=now() WHERE id=$1 AND tenant=$2`, id, tenant)
	if e == nil && r.RowsAffected() != 1 {
		return pgx.ErrNoRows
	}
	return e
}
func (s *Store) Pending(ctx context.Context) ([]string, error) {
	rows, e := s.Pool.Query(ctx, `SELECT id FROM agentenv_adapter_operations WHERE state NOT IN ('Released','Rejected') ORDER BY updated LIMIT 1000`)
	if e != nil {
		return nil, e
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		var id string
		if e = rows.Scan(&id); e != nil {
			return nil, e
		}
		out = append(out, id)
	}
	return out, rows.Err()
}

// A session advisory lock spans external calls without holding a SQL transaction.
func (s *Store) Exclusive(ctx context.Context, id string, f func() error) error {
	c, e := s.Pool.Acquire(ctx)
	if e != nil {
		return e
	}
	defer c.Release()
	var ok bool
	e = c.QueryRow(ctx, `SELECT pg_try_advisory_lock(hashtextextended($1,37))`, id).Scan(&ok)
	if e != nil {
		return e
	}
	if !ok {
		return core.ErrPending
	}
	defer func() {
		unlock, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if _, e := c.Exec(unlock, `SELECT pg_advisory_unlock(hashtextextended($1,37))`, id); e != nil {
			_ = c.Conn().Close(unlock)
		}
	}()
	return f()
}
func (s *Store) AuthorizeInitialization(ctx context.Context, ref core.PVCRef, op string) (bool, error) {
	var enabled bool
	var owner string
	tx, e := s.Pool.Begin(ctx)
	if e != nil {
		return false, e
	}
	defer tx.Rollback(ctx)
	e = tx.QueryRow(ctx, `SELECT initialize_ext4,initialization_owner FROM agentenv_adapter_volumes WHERE namespace=$1 AND name=$2 AND uid=$3 FOR UPDATE`, ref.Namespace, ref.Name, ref.UID).Scan(&enabled, &owner)
	if errors.Is(e, pgx.ErrNoRows) {
		return false, nil
	}
	if e != nil {
		return false, e
	}
	if !enabled || (owner != "" && owner != op) {
		return false, nil
	}
	_, e = tx.Exec(ctx, `UPDATE agentenv_adapter_volumes SET initialization_owner=$3 WHERE namespace=$1 AND name=$2`, ref.Namespace, ref.Name, op)
	if e != nil {
		return false, e
	}
	return true, tx.Commit(ctx)
}
