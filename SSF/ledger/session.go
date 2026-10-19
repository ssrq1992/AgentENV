package ledger

import (
	"context"
	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
	"github.com/jackc/pgx/v5/pgxpool"
)

type sessionKey struct{}
type executor interface {
	Exec(context.Context, string, ...any) (pgconn.CommandTag, error)
	Query(context.Context, string, ...any) (pgx.Rows, error)
	QueryRow(context.Context, string, ...any) pgx.Row
	Begin(context.Context) (pgx.Tx, error)
}

func (s *Store) executor(ctx context.Context) executor {
	if c, ok := ctx.Value(sessionKey{}).(*pgxpool.Conn); ok {
		return c
	}
	return s.Pool
}
