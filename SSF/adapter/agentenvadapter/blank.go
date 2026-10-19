package agentenvadapter

import (
	"context"
	"errors"
	"io"
)

// Initialization requires an actually empty fresh device, not merely the
// absence of a blkid-recognized filesystem. This one-time scan is not on reuse.
func checkZero(ctx context.Context, r io.Reader) error {
	b := make([]byte, 1<<20)
	for {
		if e := ctx.Err(); e != nil {
			return e
		}
		n, e := r.Read(b)
		for _, v := range b[:n] {
			if v != 0 {
				return errors.New("new-volume initialization refused: device contains nonzero data")
			}
		}
		if e == io.EOF {
			return nil
		}
		if e != nil {
			return e
		}
		if n == 0 {
			return io.ErrNoProgress
		}
	}
}
